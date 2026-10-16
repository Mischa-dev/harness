//! Migrations, applied in order and tracked in `PRAGMA user_version`. Never
//! edit a migration that has shipped; add a new one.

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // 1: workdays, transcripts and wakes.
    "CREATE TABLE days (
         id INTEGER PRIMARY KEY,
         started_at INTEGER NOT NULL,
         ended_at INTEGER,
         handoff TEXT,
         context_tokens INTEGER NOT NULL DEFAULT 0
     );
     -- At most one open day, even with two handles racing to start one.
     CREATE UNIQUE INDEX days_one_open ON days((ended_at IS NULL)) WHERE ended_at IS NULL;
     CREATE TABLE transcript (
         day_id INTEGER NOT NULL REFERENCES days(id),
         seq INTEGER NOT NULL,
         message TEXT NOT NULL,
         PRIMARY KEY (day_id, seq)
     );
     CREATE TABLE wakes (
         id INTEGER PRIMARY KEY,
         day_id INTEGER NOT NULL REFERENCES days(id),
         reason TEXT NOT NULL,
         started_at INTEGER NOT NULL,
         ended_at INTEGER NOT NULL,
         end_state TEXT NOT NULL,
         turns INTEGER NOT NULL,
         input_tokens INTEGER NOT NULL,
         output_tokens INTEGER NOT NULL,
         cache_read_tokens INTEGER NOT NULL,
         cache_write_tokens INTEGER NOT NULL,
         cost_usd REAL NOT NULL
     );
     CREATE INDEX wakes_started_at ON wakes(started_at);",
    // 2: the wake loop's state between wakes, so a restart keeps it.
    "CREATE TABLE rhythm (
         id INTEGER PRIMARY KEY CHECK (id = 1),
         next_look INTEGER,
         paused TEXT
     );",
    // 3: the memory snapshot the current day started from, so a restart
    // mid-day keeps the same prompt.
    "ALTER TABLE rhythm ADD COLUMN memory_snapshot TEXT;",
];

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current: usize = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }
    Ok(())
}
