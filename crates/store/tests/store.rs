use harness_providers::{ContentBlock, Message, Role, ToolCall, ToolResult, Usage};
use harness_store::{Store, StoreError, WakeRecord};
use serde_json::json;
use time::Duration;
use time::macros::datetime;

fn tool_turn() -> Vec<Message> {
    vec![
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Opaque {
                    provider: "anthropic".into(),
                    data: json!({"type": "thinking", "thinking": "", "signature": "c2ln"}),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "t1".into(),
                    name: "sleep".into(),
                    arguments: json!({"minutes": 60}),
                }),
            ],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult(ToolResult {
                call_id: "t1".into(),
                content: "Sleeping.".into(),
                is_error: false,
            })],
        },
    ]
}

fn wake(day_id: i64, started_at: time::OffsetDateTime, cost_usd: f64) -> WakeRecord {
    WakeRecord {
        day_id,
        reason: r#"{"kind":"next_look"}"#.into(),
        started_at,
        ended_at: started_at + Duration::minutes(2),
        end: "slept".into(),
        turns: 2,
        usage: Usage {
            input_tokens: 300,
            output_tokens: 40,
            cache_read_tokens: 12_000,
            cache_write_tokens: 500,
        },
        cost_usd,
    }
}

#[test]
fn a_transcript_survives_reopening_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let start = datetime!(2026-01-05 09:00 UTC);
    let mut expected = vec![Message::user_text("Wake: a DM from Mischa.")];
    expected.extend(tool_turn());

    let day_id = {
        let mut store = Store::open(&path).unwrap();
        let day = store.start_day(start).unwrap();
        store.append(day.id, &expected[..1]).unwrap();
        store.append(day.id, &expected[1..]).unwrap();
        day.id
    };

    let store = Store::open(&path).unwrap();
    let day = store.current_day().unwrap().expect("day still open");
    assert_eq!(day.id, day_id);
    assert_eq!(day.started_at, start);
    assert_eq!(store.transcript(day_id).unwrap(), expected);
}

#[test]
fn one_day_at_a_time_and_handoffs_carry_over() {
    let mut store = Store::open_in_memory().unwrap();
    let monday = store.start_day(datetime!(2026-01-05 09:00 UTC)).unwrap();
    assert!(store.start_day(datetime!(2026-01-05 10:00 UTC)).is_err());
    assert_eq!(store.last_handoff().unwrap(), None);

    store
        .end_day(
            monday.id,
            datetime!(2026-01-06 02:00 UTC),
            Some("Waiting on Mischa's review of #42."),
        )
        .unwrap();
    assert!(matches!(
        store.append(monday.id, &[Message::user_text("late")]),
        Err(StoreError::DayEnded(_))
    ));
    assert!(matches!(
        store.end_day(monday.id, datetime!(2026-01-06 03:00 UTC), None),
        Err(StoreError::DayEnded(_))
    ));
    assert_eq!(store.current_day().unwrap(), None);
    assert_eq!(
        store.last_handoff().unwrap().as_deref(),
        Some("Waiting on Mischa's review of #42.")
    );

    let tuesday = store.start_day(datetime!(2026-01-06 08:00 UTC)).unwrap();
    assert!(store.transcript(tuesday.id).unwrap().is_empty());
}

#[test]
fn wakes_record_usage_cost_and_the_days_context_size() {
    let mut store = Store::open_in_memory().unwrap();
    let start = datetime!(2026-01-05 09:00 UTC);
    let day = store.start_day(start).unwrap();
    store
        .record_wake(&wake(day.id, start, 0.05), 12_840)
        .unwrap();
    store
        .record_wake(&wake(day.id, start + Duration::hours(3), 0.07), 15_100)
        .unwrap();

    assert_eq!(store.current_day().unwrap().unwrap().context_tokens, 15_100);
    let all = store.wakes_since(start).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0], wake(day.id, start, 0.05));
    assert!((store.spend_since(start).unwrap() - 0.12).abs() < 1e-9);
    assert!((store.spend_since(start + Duration::hours(1)).unwrap() - 0.07).abs() < 1e-9);
}

#[test]
fn migrations_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    drop(Store::open(&path).unwrap());
    drop(Store::open(&path).unwrap());
    let store = Store::open(&path).unwrap();
    assert_eq!(store.current_day().unwrap(), None);
}

#[test]
fn the_days_context_size_never_shrinks() {
    let mut store = Store::open_in_memory().unwrap();
    let start = datetime!(2026-01-05 09:00 UTC);
    let day = store.start_day(start).unwrap();
    store
        .record_wake(&wake(day.id, start, 0.05), 70_000)
        .unwrap();
    // A provider that reports no usage must not hide how large the day is.
    store.record_wake(&wake(day.id, start, 0.0), 0).unwrap();
    assert_eq!(store.current_day().unwrap().unwrap().context_tokens, 70_000);
}

#[test]
fn a_wake_is_saved_whole_or_not_at_all() {
    let mut store = Store::open_in_memory().unwrap();
    let start = datetime!(2026-01-05 09:00 UTC);
    let day = store.start_day(start).unwrap();
    store.end_day(day.id, start, None).unwrap();
    // The day ended underneath the wake: neither the messages nor the record land.
    assert!(matches!(
        store.finish_wake(&tool_turn(), &wake(day.id, start, 0.05), 500),
        Err(StoreError::DayEnded(_))
    ));
    assert!(store.transcript(day.id).unwrap().is_empty());
    assert!(store.wakes_since(start).unwrap().is_empty());
}
