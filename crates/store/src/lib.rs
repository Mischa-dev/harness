//! The agent's runtime state in SQLite (WAL), kept in its Agent OS state
//! folder: workdays, each day's transcript, and a record of every wake.

mod schema;
pub mod workday;

use std::path::Path;

use harness_providers::{Message, Usage};
use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;

pub use workday::{EveningReason, WorkdayPolicy};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
    #[error("day {0} has already ended")]
    DayEnded(i64),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// One workday: one continuous conversation, from the morning start to the
/// evening turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    pub id: i64,
    pub started_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
    /// The note the agent left for tomorrow, written during the evening turn.
    pub handoff: Option<String>,
    /// The context size of the day's latest model call, in tokens.
    pub context_tokens: u64,
}

/// What a finished wake leaves behind, for budgets, breakers and the state view.
#[derive(Debug, Clone, PartialEq)]
pub struct WakeRecord {
    pub day_id: i64,
    /// The wake reason, as JSON.
    pub reason: String,
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
    /// How the wake ended, e.g. `slept`, `paused`, `turn_limit`.
    pub end: String,
    pub turns: u32,
    pub usage: Usage,
    pub cost_usd: f64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// The day in progress, if one has started and not ended.
    pub fn current_day(&self) -> Result<Option<Day>> {
        self.conn
            .query_row(
                "SELECT id, started_at, ended_at, handoff, context_tokens
                 FROM days WHERE ended_at IS NULL ORDER BY id DESC LIMIT 1",
                [],
                day_from_row,
            )
            .optional()?
            .transpose()
    }

    /// Starts a new day. Only one day can be open at a time.
    pub fn start_day(&self, now: OffsetDateTime) -> Result<Day> {
        if let Some(open) = self.current_day()? {
            return Err(StoreError::Corrupt(format!(
                "day {} is still open; end it before starting another",
                open.id
            )));
        }
        self.conn.execute(
            "INSERT INTO days (started_at, context_tokens) VALUES (?1, 0)",
            params![now.unix_timestamp()],
        )?;
        Ok(Day {
            id: self.conn.last_insert_rowid(),
            started_at: now,
            ended_at: None,
            handoff: None,
            context_tokens: 0,
        })
    }

    /// Closes the day after its evening turn, keeping the handoff for tomorrow.
    pub fn end_day(&self, day_id: i64, now: OffsetDateTime, handoff: Option<&str>) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE days SET ended_at = ?2, handoff = ?3 WHERE id = ?1 AND ended_at IS NULL",
            params![day_id, now.unix_timestamp(), handoff],
        )?;
        if changed == 0 {
            return Err(StoreError::DayEnded(day_id));
        }
        Ok(())
    }

    /// The handoff note from the most recent ended day.
    pub fn last_handoff(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT handoff FROM days WHERE ended_at IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Appends messages to the day's transcript, in order, in one transaction.
    pub fn append(&mut self, day_id: i64, messages: &[Message]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let ended: Option<Option<i64>> = tx
            .query_row(
                "SELECT ended_at FROM days WHERE id = ?1",
                params![day_id],
                |row| row.get(0),
            )
            .optional()?;
        match ended {
            None => return Err(StoreError::Corrupt(format!("no day {day_id}"))),
            Some(Some(_)) => return Err(StoreError::DayEnded(day_id)),
            Some(None) => {}
        }
        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM transcript WHERE day_id = ?1",
            params![day_id],
            |row| row.get(0),
        )?;
        {
            let mut insert =
                tx.prepare("INSERT INTO transcript (day_id, seq, message) VALUES (?1, ?2, ?3)")?;
            for (i, message) in messages.iter().enumerate() {
                let json = serde_json::to_string(message)
                    .map_err(|e| StoreError::Corrupt(e.to_string()))?;
                insert.execute(params![day_id, next + i as i64, json])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn transcript(&self, day_id: i64) -> Result<Vec<Message>> {
        let mut query = self
            .conn
            .prepare("SELECT message FROM transcript WHERE day_id = ?1 ORDER BY seq")?;
        let rows = query.query_map(params![day_id], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let json = row?;
            serde_json::from_str(&json).map_err(|e| StoreError::Corrupt(e.to_string()))
        })
        .collect()
    }

    /// Records a finished wake, and the context size its last call reached.
    pub fn record_wake(&self, wake: &WakeRecord, context_tokens: u64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO wakes (day_id, reason, started_at, ended_at, end_state, turns,
                                input_tokens, output_tokens, cache_read_tokens,
                                cache_write_tokens, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                wake.day_id,
                wake.reason,
                wake.started_at.unix_timestamp(),
                wake.ended_at.unix_timestamp(),
                wake.end,
                wake.turns,
                wake.usage.input_tokens as i64,
                wake.usage.output_tokens as i64,
                wake.usage.cache_read_tokens as i64,
                wake.usage.cache_write_tokens as i64,
                wake.cost_usd,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "UPDATE days SET context_tokens = ?2 WHERE id = ?1",
            params![wake.day_id, context_tokens as i64],
        )?;
        Ok(id)
    }

    /// Wakes that started at or after `since`, oldest first.
    pub fn wakes_since(&self, since: OffsetDateTime) -> Result<Vec<WakeRecord>> {
        let mut query = self.conn.prepare(
            "SELECT day_id, reason, started_at, ended_at, end_state, turns, input_tokens,
                    output_tokens, cache_read_tokens, cache_write_tokens, cost_usd
             FROM wakes WHERE started_at >= ?1 ORDER BY id",
        )?;
        let rows = query.query_map(params![since.unix_timestamp()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u32>(5)?,
                [
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ],
                row.get::<_, f64>(10)?,
            ))
        })?;
        rows.map(|row| {
            let (day_id, reason, started, ended, end, turns, tokens, cost_usd) = row?;
            Ok(WakeRecord {
                day_id,
                reason,
                started_at: timestamp(started)?,
                ended_at: timestamp(ended)?,
                end,
                turns,
                usage: Usage {
                    input_tokens: tokens[0] as u64,
                    output_tokens: tokens[1] as u64,
                    cache_read_tokens: tokens[2] as u64,
                    cache_write_tokens: tokens[3] as u64,
                },
                cost_usd,
            })
        })
        .collect()
    }

    /// Model spend for wakes that started at or after `since`.
    pub fn spend_since(&self, since: OffsetDateTime) -> Result<f64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM wakes WHERE started_at >= ?1",
            params![since.unix_timestamp()],
            |row| row.get(0),
        )?)
    }
}

fn timestamp(seconds: i64) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(seconds).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn day_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Day>> {
    let id: i64 = row.get(0)?;
    let started: i64 = row.get(1)?;
    let ended: Option<i64> = row.get(2)?;
    let handoff: Option<String> = row.get(3)?;
    let context_tokens: i64 = row.get(4)?;
    Ok((|| {
        Ok(Day {
            id,
            started_at: timestamp(started)?,
            ended_at: ended.map(timestamp).transpose()?,
            handoff,
            context_tokens: context_tokens as u64,
        })
    })())
}
