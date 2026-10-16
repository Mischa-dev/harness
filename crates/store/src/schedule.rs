//! The agent's own clock: follow-ups it sets itself ("check tomorrow whether
//! she looked at #42") and routines it decides it wants ("Friday afternoon:
//! look over dependency updates"). A follow-up fires once and is gone; a
//! routine moves on to its next time.
//!
//! The book is its own handle on the state file, so the agent's tools and
//! the wake loop can share it while a wake holds the main store.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use time::{Duration, OffsetDateTime, UtcOffset, Weekday};

use crate::{Result, StoreError, open_file, prepare, timestamp};

/// How often a routine comes round, in the owner's time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Daily,
    /// Monday to Friday.
    Weekdays,
    Weekly,
}

impl Repeat {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "daily" => Some(Self::Daily),
            "weekdays" => Some(Self::Weekdays),
            "weekly" => Some(Self::Weekly),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekdays => "weekdays",
            Self::Weekly => "weekly",
        }
    }

    /// The first time this routine comes round after `now`, keeping the
    /// time of day `due` was set for. A routine missed while the agent was
    /// down fires once, not once per time it missed.
    pub fn next_after(
        self,
        due: OffsetDateTime,
        now: OffsetDateTime,
        offset: UtcOffset,
    ) -> OffsetDateTime {
        let period = match self {
            Self::Weekly => Duration::weeks(1),
            Self::Daily | Self::Weekdays => Duration::days(1),
        };
        let mut next = due + period;
        if next <= now {
            let behind = (now - next).whole_seconds() / period.whole_seconds() + 1;
            next += period * behind as i32;
        }
        self.settle(next, offset)
    }

    /// Moves a weekday routine off the weekend, to Monday.
    pub fn settle(self, at: OffsetDateTime, offset: UtcOffset) -> OffsetDateTime {
        let mut at = at;
        if self == Self::Weekdays {
            while matches!(
                at.to_offset(offset).weekday(),
                Weekday::Saturday | Weekday::Sunday
            ) {
                at += Duration::days(1);
            }
        }
        at
    }
}

/// One follow-up or routine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduled {
    pub id: i64,
    pub note: String,
    pub due_at: OffsetDateTime,
    /// `None` for a follow-up.
    pub repeat: Option<Repeat>,
}

pub struct ScheduleBook {
    conn: Mutex<Connection>,
}

impl ScheduleBook {
    /// Opens the schedule in the state file at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: Mutex::new(prepare(open_file(path)?)?),
        })
    }

    /// A schedule of its own in memory, for tests and simulations.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            conn: Mutex::new(prepare(Connection::open_in_memory()?)?),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn add(
        &self,
        note: &str,
        due_at: OffsetDateTime,
        repeat: Option<Repeat>,
        now: OffsetDateTime,
    ) -> Result<Scheduled> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO schedules (note, due_at, repeat, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                note,
                due_at.unix_timestamp(),
                repeat.map(Repeat::as_str),
                now.unix_timestamp()
            ],
        )?;
        Ok(Scheduled {
            id: conn.last_insert_rowid(),
            note: note.to_string(),
            due_at: timestamp(due_at.unix_timestamp())?,
            repeat,
        })
    }

    /// Removes one entry, returning it if it was there.
    pub fn remove(&self, id: i64) -> Result<Option<Scheduled>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                "SELECT id, note, due_at, repeat FROM schedules WHERE id = ?1",
                params![id],
                raw_row,
            )
            .optional()?
            .map(scheduled)
            .transpose()?;
        conn.execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
        Ok(found)
    }

    /// Everything scheduled, soonest first.
    pub fn list(&self) -> Result<Vec<Scheduled>> {
        let conn = self.conn();
        let mut select =
            conn.prepare("SELECT id, note, due_at, repeat FROM schedules ORDER BY due_at, id")?;
        let rows = select.query_map([], raw_row)?;
        rows.map(|row| scheduled(row?)).collect()
    }

    /// When the soonest entry comes due.
    pub fn next_due(&self) -> Result<Option<OffsetDateTime>> {
        self.conn()
            .query_row("SELECT MIN(due_at) FROM schedules", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .map(timestamp)
            .transpose()
    }

    /// Takes everything due at `now`, as it was when it came due: follow-ups
    /// are removed and routines move on to their next time, in `offset`'s
    /// days. Done in one transaction, so nothing fires twice.
    pub fn take_due(&self, now: OffsetDateTime, offset: UtcOffset) -> Result<Vec<Scheduled>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let due = {
            let mut select = tx.prepare(
                "SELECT id, note, due_at, repeat FROM schedules WHERE due_at <= ?1
                 ORDER BY due_at, id",
            )?;
            let rows = select.query_map(params![now.unix_timestamp()], raw_row)?;
            rows.map(|row| scheduled(row?))
                .collect::<Result<Vec<_>>>()?
        };
        for entry in &due {
            match entry.repeat {
                None => {
                    tx.execute("DELETE FROM schedules WHERE id = ?1", params![entry.id])?;
                }
                Some(repeat) => {
                    let next = repeat.next_after(entry.due_at, now, offset);
                    tx.execute(
                        "UPDATE schedules SET due_at = ?1 WHERE id = ?2",
                        params![next.unix_timestamp(), entry.id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(due)
    }
}

type RawRow = (i64, String, i64, Option<String>);

fn raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn scheduled((id, note, due_at, repeat): RawRow) -> Result<Scheduled> {
    let repeat = repeat
        .map(|text| {
            Repeat::parse(&text)
                .ok_or_else(|| StoreError::Corrupt(format!("unknown repeat {text:?}")))
        })
        .transpose()?;
    Ok(Scheduled {
        id,
        note,
        due_at: timestamp(due_at)?,
        repeat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{datetime, offset};

    #[test]
    fn follow_ups_fire_once_and_routines_come_round() {
        let book = ScheduleBook::open_in_memory().unwrap();
        let now = datetime!(2026-10-16 09:00 UTC);
        let once = book
            .add("did Mara look at #42?", now + Duration::hours(2), None, now)
            .unwrap();
        let weekly = book
            .add(
                "dependency updates",
                datetime!(2026-10-16 15:00 UTC),
                Some(Repeat::Weekly),
                now,
            )
            .unwrap();
        assert_eq!(book.next_due().unwrap(), Some(now + Duration::hours(2)));
        assert!(book.take_due(now, UtcOffset::UTC).unwrap().is_empty());

        let at_noon = book.take_due(datetime!(2026-10-16 12:00 UTC), UtcOffset::UTC);
        assert_eq!(at_noon.unwrap(), vec![once]);
        assert_eq!(book.list().unwrap(), vec![weekly.clone()]);

        // Down for a fortnight: the routine fires once, then keeps its time.
        let later = book
            .take_due(datetime!(2026-11-01 10:00 UTC), UtcOffset::UTC)
            .unwrap();
        assert_eq!(later, vec![weekly.clone()]);
        assert_eq!(
            book.next_due().unwrap(),
            Some(datetime!(2026-11-06 15:00 UTC))
        );
        assert!(book.remove(weekly.id).unwrap().is_some());
        assert!(book.remove(weekly.id).unwrap().is_none());
        assert_eq!(book.next_due().unwrap(), None);
    }

    #[test]
    fn weekday_routines_skip_the_owners_weekend() {
        // Friday 23:30 UTC is already Saturday at +2.
        let friday = datetime!(2026-10-16 23:30 UTC);
        assert_eq!(
            Repeat::Weekdays.settle(friday, offset!(+2)),
            datetime!(2026-10-18 23:30 UTC)
        );
        assert_eq!(Repeat::Weekdays.settle(friday, UtcOffset::UTC), friday);
        assert_eq!(
            Repeat::Weekdays.next_after(friday, friday, UtcOffset::UTC),
            datetime!(2026-10-19 23:30 UTC)
        );
        assert_eq!(
            Repeat::Daily.next_after(friday, friday + Duration::hours(30), UtcOffset::UTC),
            datetime!(2026-10-18 23:30 UTC)
        );
    }

    #[test]
    fn the_schedule_shares_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let store = crate::Store::open(&path).unwrap();
        let book = ScheduleBook::open(&path).unwrap();
        let now = datetime!(2026-10-16 09:00 UTC);
        book.add("look again", now, None, now).unwrap();
        drop(book);
        assert_eq!(
            ScheduleBook::open(&path).unwrap().list().unwrap()[0].note,
            "look again"
        );
        drop(store);
    }
}
