//! The simulator end to end: bundled scenarios with the fake teammate, and
//! agents that misbehave on purpose so each check is seen to fail.

use async_trait::async_trait;
use harness_providers::{
    ContentBlock, Message, ModelRequest, ModelResponse, Provider, ProviderError, Role, StopReason,
    ToolCall, Usage,
};
use serde_json::{Value, json};
use sim::{FakeTeammate, Report, Scenario, fake_profile, find_scenario, run, scenarios_dir};

async fn run_with(scenario: &Scenario, provider: &dyn Provider) -> Report {
    run(scenario, provider, &fake_profile(), "fake")
        .await
        .unwrap()
}

fn bundled(name: &str) -> Scenario {
    Scenario::load(&find_scenario(name)).unwrap()
}

fn failed(report: &Report) -> Vec<&str> {
    report
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name.as_str())
        .collect()
}

/// Answers every wake with the same tool calls.
struct Always(Vec<(&'static str, Value)>);

#[async_trait]
impl Provider for Always {
    async fn complete(&self, _: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: self
                    .0
                    .iter()
                    .enumerate()
                    .map(|(i, (name, args))| {
                        ContentBlock::ToolCall(ToolCall {
                            id: format!("c{i}"),
                            name: (*name).into(),
                            arguments: args.clone(),
                        })
                    })
                    .collect(),
            },
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        })
    }
}

fn sleep() -> (&'static str, Value) {
    ("sleep", json!({"minutes": 480}))
}

#[tokio::test]
async fn every_bundled_scenario_passes_with_the_fake_teammate() {
    let mut names = 0;
    for entry in std::fs::read_dir(scenarios_dir()).unwrap() {
        let path = entry.unwrap().path();
        let scenario = Scenario::load(&path).unwrap();
        let report = run_with(&scenario, &FakeTeammate::default()).await;
        assert!(report.passed(), "{report}");
        names += 1;
    }
    assert!(names >= 2);
}

#[tokio::test]
async fn chatter_not_addressed_to_the_agent_doesnt_wake_it() {
    let scenario = bundled("busy-group-chat");
    let report = run_with(&scenario, &FakeTeammate::default()).await;
    // Fifteen messages arrive, two of them mentions. The agent wakes for the
    // mentions and its own looks, not for each message.
    assert_eq!(scenario.events.len(), 15);
    assert!(report.wakes < 8, "{report}");
    assert_eq!(report.sent.len(), 2);
}

#[tokio::test]
async fn an_agent_that_ignores_a_dm_fails() {
    let report = run_with(&bundled("quiet-week"), &Always(vec![sleep()])).await;
    assert_eq!(failed(&report), ["answers what is addressed to it"]);
}

#[tokio::test]
async fn an_agent_that_chimes_in_everywhere_fails() {
    let chatty = Always(vec![
        (
            "message_send",
            json!({"conversation": "#general", "text": "Just checking in!"}),
        ),
        ("message_send", json!({"conversation": "dm", "text": "Hi!"})),
        sleep(),
    ]);
    let report = run_with(&bundled("quiet-week"), &chatty).await;
    assert_eq!(failed(&report), ["stays quiet"]);
}

#[tokio::test]
async fn a_spamming_agent_trips_the_breaker_and_the_run_stops() {
    let mut calls: Vec<_> = (0..40)
        .map(|_| {
            (
                "message_send",
                json!({"conversation": "dm", "text": "ping"}),
            )
        })
        .collect();
    calls.push(sleep());
    let report = run_with(&bundled("quiet-week"), &Always(calls)).await;
    let failed = failed(&report);
    for check in ["hits no walls", "never pauses", "stays quiet"] {
        assert!(failed.contains(&check), "{report}");
    }
    assert_eq!(report.wakes, 1, "a paused agent doesn't wake again");
    assert_eq!(report.sent.len(), 30, "the breaker stops the 31st message");
}

/// Replies only to the newest wake message's last addressed line, if it
/// mentions the release notes, then sleeps.
struct AnswersOnlyTheSecondMention;

#[async_trait]
impl Provider for AnswersOnlyTheSecondMention {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let last = serde_json::to_string(request.messages.last().unwrap()).unwrap();
        let mut calls = vec![];
        if last.contains("release notes from the merged PRs") {
            calls.push((
                "message_send",
                json!({"conversation": "#general", "text": "Drafting them now."}),
            ));
        }
        calls.push(sleep());
        Always(calls).complete(request).await
    }
}

#[tokio::test]
async fn a_late_reply_doesnt_cover_an_ignored_mention() {
    let report = run_with(&bundled("busy-group-chat"), &AnswersOnlyTheSecondMention).await;
    assert_eq!(failed(&report), ["answers what is addressed to it"]);
    assert_eq!(report.sent.len(), 1);
}

#[tokio::test]
async fn a_wake_without_sleep_counts_as_failed() {
    let report = run_with(&bundled("busy-group-chat"), &Always(vec![])).await;
    let failed = failed(&report);
    assert!(failed.contains(&"no failed wakes"), "{report}");
}

#[tokio::test]
async fn a_run_cut_short_by_the_wake_cap_fails() {
    // Five-minute sleeps for a week is far more than the cap allows.
    let restless = Always(vec![("sleep", json!({"minutes": 5}))]);
    let report = run_with(&bundled("quiet-week"), &restless).await;
    let failed = failed(&report);
    assert!(failed.contains(&"no failed wakes"), "{report}");
}

#[tokio::test]
async fn blank_replies_dont_count() {
    let blank = Always(vec![
        ("message_send", json!({"conversation": "dm", "text": "  "})),
        sleep(),
    ]);
    let report = run_with(&bundled("quiet-week"), &blank).await;
    assert!(report.sent.is_empty());
    assert_eq!(failed(&report), ["answers what is addressed to it"]);
}
