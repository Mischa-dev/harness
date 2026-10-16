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

    // The wake filter's calls count too.
    store
        .record_helper_call(start + Duration::hours(2), "wake_filter", 0.002)
        .unwrap();
    assert!((store.spend_since(start).unwrap() - 0.122).abs() < 1e-9);
    assert!((store.spend_since(start + Duration::hours(3)).unwrap() - 0.07).abs() < 1e-9);
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

#[cfg(unix)]
#[test]
fn state_database_is_private_and_symlinks_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    drop(Store::open(&path).unwrap());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let link = dir.path().join("linked.sqlite");
    symlink(&path, &link).unwrap();
    assert!(matches!(Store::open(&link), Err(StoreError::Io(_))));
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
fn oversized_provider_counters_do_not_wrap_negative_in_sqlite() {
    let mut store = Store::open_in_memory().unwrap();
    let start = datetime!(2026-01-05 09:00 UTC);
    let day = store.start_day(start).unwrap();
    let mut record = wake(day.id, start, 0.0);
    record.usage = Usage {
        input_tokens: u64::MAX,
        output_tokens: u64::MAX,
        cache_read_tokens: u64::MAX,
        cache_write_tokens: u64::MAX,
    };
    store.record_wake(&record, u64::MAX).unwrap();

    let largest_sqlite_integer = i64::MAX as u64;
    let saved = &store.wakes_since(start).unwrap()[0];
    assert_eq!(saved.usage.input_tokens, largest_sqlite_integer);
    assert_eq!(saved.usage.output_tokens, largest_sqlite_integer);
    assert_eq!(saved.usage.cache_read_tokens, largest_sqlite_integer);
    assert_eq!(saved.usage.cache_write_tokens, largest_sqlite_integer);
    assert_eq!(
        store.current_day().unwrap().unwrap().context_tokens,
        largest_sqlite_integer
    );
}

#[test]
fn a_wake_is_saved_whole_or_not_at_all() {
    let mut store = Store::open_in_memory().unwrap();
    let start = datetime!(2026-01-05 09:00 UTC);
    let day = store.start_day(start).unwrap();
    let id = store.begin_wake(&wake(day.id, start, 0.0)).unwrap();
    store.end_day(day.id, start, None).unwrap();
    // The day ended underneath the wake: neither the messages nor the update land.
    assert!(matches!(
        store.save_wake(id, 0, &tool_turn(), &wake(day.id, start, 0.05), 500),
        Err(StoreError::DayEnded(_))
    ));
    assert!(store.transcript(day.id).unwrap().is_empty());
    assert_eq!(store.wakes_since(start).unwrap()[0].cost_usd, 0.0);
}

#[test]
fn a_wake_cut_off_mid_way_keeps_its_saved_turns_and_spend() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let start = datetime!(2026-01-05 09:00 UTC);
    {
        let mut store = Store::open(&path).unwrap();
        let day = store.start_day(start).unwrap();
        let mut running = wake(day.id, start, 0.0);
        running.end = "running".into();
        let id = store.begin_wake(&running).unwrap();
        running.cost_usd = 0.03;
        store
            .save_wake(id, 0, &tool_turn(), &running, 9_000)
            .unwrap();
        // The process dies here, before the wake's final save.
    }
    let store = Store::open(&path).unwrap();
    let day = store.current_day().unwrap().unwrap();
    assert_eq!(store.transcript(day.id).unwrap(), tool_turn());
    assert_eq!(day.context_tokens, 9_000);
    let wakes = store.wakes_since(start).unwrap();
    assert_eq!(wakes[0].end, "running");
    assert!((store.spend_since(start).unwrap() - 0.03).abs() < 1e-9);
}

#[test]
fn two_handles_cannot_both_open_a_day() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let first = Store::open(&path).unwrap();
    let second = Store::open(&path).unwrap();
    let start = datetime!(2026-01-05 09:00 UTC);
    first.start_day(start).unwrap();
    assert!(second.start_day(start).is_err());
    // A raw insert that skips the check still hits the constraint.
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert!(
        conn.execute("INSERT INTO days (started_at) VALUES (0)", [])
            .is_err()
    );
}

#[test]
fn a_later_save_replaces_a_pending_message() {
    let mut store = Store::open_in_memory().unwrap();
    let start = datetime!(2026-01-05 09:00 UTC);
    let day = store.start_day(start).unwrap();
    let record = wake(day.id, start, 0.0);
    let id = store.begin_wake(&record).unwrap();
    let turn = tool_turn();
    let pending = Message::user_text("cut off");
    store
        .save_wake_with_pending(id, 0, &turn[..1], Some(&pending), &record, 0)
        .unwrap();
    assert_eq!(
        store.transcript(day.id).unwrap(),
        [&turn[..1], &[pending]].concat()
    );
    store.save_wake(id, 1, &turn[1..], &record, 0).unwrap();
    assert_eq!(store.transcript(day.id).unwrap(), turn);
    // A save can't leave a gap in the transcript.
    assert!(matches!(
        store.save_wake(id, 5, &turn, &record, 0),
        Err(StoreError::Corrupt(_))
    ));
}
