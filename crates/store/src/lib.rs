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
    #[error("state file error: {0}")]
    Io(#[from] std::io::Error),
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
        secure_state_file(path)?;
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let conn = Connection::open_with_flags(path, flags)?;
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
        let still_open = |id: i64| {
            StoreError::Corrupt(format!(
                "day {id} is still open; end it before starting another"
            ))
        };
        if let Some(open) = self.current_day()? {
            return Err(still_open(open.id));
        }
        // The days_one_open index catches a second handle that raced past the check.
        match self.conn.execute(
            "INSERT INTO days (started_at, context_tokens) VALUES (?1, 0)",
            params![now.unix_timestamp()],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                let open = self.current_day()?.map_or(0, |d| d.id);
                return Err(still_open(open));
            }
            Err(e) => return Err(e.into()),
        }
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
        append_in(&tx, day_id, messages)?;
        tx.commit()?;
        Ok(())
    }

    /// How many messages the day's transcript holds.
    pub fn transcript_len(&self, day_id: i64) -> Result<usize> {
        let len: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM transcript WHERE day_id = ?1",
            params![day_id],
            |row| row.get(0),
        )?;
        Ok(len as usize)
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
    pub fn record_wake(&mut self, wake: &WakeRecord, context_tokens: u64) -> Result<i64> {
        let id = self.begin_wake(wake)?;
        let from = self.transcript_len(wake.day_id)?;
        self.save_wake(id, from, &[], wake, context_tokens)?;
        Ok(id)
    }

    /// Opens the record for a wake that is starting, before any model call,
    /// so its spend is counted even if the process dies partway through.
    pub fn begin_wake(&mut self, wake: &WakeRecord) -> Result<i64> {
        let usage = usage_to_sql(&wake.usage);
        let tx = self.conn.transaction()?;
        append_in(&tx, wake.day_id, &[])?;
        tx.execute(
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
                usage[0],
                usage[1],
                usage[2],
                usage[3],
                wake.cost_usd,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Saves a wake's progress: the day's transcript from position `from` on,
    /// replacing whatever was saved there before (a batch of tool calls is
    /// saved as it runs, then saved again once its results are in), and the
    /// wake's record so far. Each save is one transaction, so a crash can't
    /// leave a conversation whose spend was never counted. The day's context
    /// size only grows: a call that reports fewer tokens (or none) doesn't
    /// hide how large the day already is.
    pub fn save_wake(
        &mut self,
        wake_id: i64,
        from: usize,
        messages: &[Message],
        wake: &WakeRecord,
        context_tokens: u64,
    ) -> Result<()> {
        self.save_wake_with_pending(wake_id, from, messages, None, wake, context_tokens)
    }

    /// Like [`Store::save_wake`], with one provisional message appended after
    /// `messages`. Keeping it separate lets checkpoints persist a synthesized
    /// tool-results message without cloning the transcript slice first.
    pub fn save_wake_with_pending(
        &mut self,
        wake_id: i64,
        from: usize,
        messages: &[Message],
        pending: Option<&Message>,
        wake: &WakeRecord,
        context_tokens: u64,
    ) -> Result<()> {
        let from_sql = i64::try_from(from)
            .map_err(|_| StoreError::Corrupt("transcript position is too large".into()))?;
        let usage = usage_to_sql(&wake.usage);
        let context_tokens = sqlite_counter(context_tokens);
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM transcript WHERE day_id = ?1 AND seq >= ?2",
            params![wake.day_id, from_sql],
        )?;
        let next = transcript_position(&tx, wake.day_id)?;
        if next as usize != from {
            return Err(StoreError::Corrupt(format!(
                "day {} has saved messages ending at {next}; can't save from {from}",
                wake.day_id
            )));
        }
        append_at(&tx, wake.day_id, from_sql, messages)?;
        if let Some(message) = pending {
            append_at(
                &tx,
                wake.day_id,
                from_sql + messages.len() as i64,
                std::slice::from_ref(message),
            )?;
        }
        let changed = tx.execute(
            "UPDATE wakes SET ended_at = ?3, end_state = ?4, turns = ?5, input_tokens = ?6,
                              output_tokens = ?7, cache_read_tokens = ?8,
                              cache_write_tokens = ?9, cost_usd = ?10
             WHERE id = ?1 AND day_id = ?2",
            params![
                wake_id,
                wake.day_id,
                wake.ended_at.unix_timestamp(),
                wake.end,
                wake.turns,
                usage[0],
                usage[1],
                usage[2],
                usage[3],
                wake.cost_usd,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Corrupt(format!(
                "no wake {wake_id} in day {}",
                wake.day_id
            )));
        }
        tx.execute(
            "UPDATE days SET context_tokens = MAX(context_tokens, ?2) WHERE id = ?1",
            params![wake.day_id, context_tokens],
        )?;
        tx.commit()?;
        Ok(())
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
                    input_tokens: nonnegative(tokens[0], "input_tokens")?,
                    output_tokens: nonnegative(tokens[1], "output_tokens")?,
                    cache_read_tokens: nonnegative(tokens[2], "cache_read_tokens")?,
                    cache_write_tokens: nonnegative(tokens[3], "cache_write_tokens")?,
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

/// Transcripts can contain private conversation and tool output. Ensure the
/// database isn't created using a permissive process umask, and refuse a
/// final-component symlink so an attacker cannot redirect state writes.
fn secure_state_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
                    return Err(error.into());
                }
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Appends to an open day's transcript inside the caller's transaction.
fn append_in(tx: &rusqlite::Transaction<'_>, day_id: i64, messages: &[Message]) -> Result<()> {
    let next = transcript_position(tx, day_id)?;
    append_at(tx, day_id, next, messages)
}

/// Returns the sequence number after the last transcript row, while also
/// verifying that the day exists and is still open. `MAX` uses the transcript
/// primary-key index instead of scanning every message as `COUNT` would.
fn transcript_position(tx: &rusqlite::Transaction<'_>, day_id: i64) -> Result<i64> {
    let position: Option<(Option<i64>, i64)> = tx
        .query_row(
            "SELECT ended_at,
                    (SELECT COALESCE(MAX(seq) + 1, 0) FROM transcript WHERE day_id = ?1)
             FROM days WHERE id = ?1",
            params![day_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match position {
        None => Err(StoreError::Corrupt(format!("no day {day_id}"))),
        Some((Some(_), _)) => Err(StoreError::DayEnded(day_id)),
        Some((None, next)) => Ok(next),
    }
}

fn append_at(
    tx: &rusqlite::Transaction<'_>,
    day_id: i64,
    next: i64,
    messages: &[Message],
) -> Result<()> {
    let mut insert =
        tx.prepare("INSERT INTO transcript (day_id, seq, message) VALUES (?1, ?2, ?3)")?;
    for (i, message) in messages.iter().enumerate() {
        let json =
            serde_json::to_string(message).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        insert.execute(params![day_id, next + i as i64, json])?;
    }
    Ok(())
}

fn timestamp(seconds: i64) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(seconds).map_err(|e| StoreError::Corrupt(e.to_string()))
}

/// SQLite integers are signed. Provider counters are unsigned and untrusted,
/// so clamp values that do not fit rather than wrapping them negative and
/// defeating usage or context accounting.
fn sqlite_counter(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn usage_to_sql(usage: &Usage) -> [i64; 4] {
    [
        sqlite_counter(usage.input_tokens),
        sqlite_counter(usage.output_tokens),
        sqlite_counter(usage.cache_read_tokens),
        sqlite_counter(usage.cache_write_tokens),
    ]
}

fn nonnegative(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Corrupt(format!("negative {field}")))
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
            context_tokens: nonnegative(context_tokens, "context_tokens")?,
        })
    })())
}
