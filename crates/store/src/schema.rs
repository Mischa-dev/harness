//! Migrations, applied in order and tracked in `PRAGMA user_version`. Never
//! edit a migration that has shipped; add a new one.

use rusqlite::Connection;

pub(crate) const MIGRATIONS: &[&str] = &[
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
    // 4: the agent's own follow-ups and routines.
    "CREATE TABLE schedules (
         id INTEGER PRIMARY KEY,
         note TEXT NOT NULL,
         due_at INTEGER NOT NULL,
         repeat TEXT CHECK (repeat IN ('daily', 'weekdays', 'weekly')),
         created_at INTEGER NOT NULL
     );
     CREATE INDEX schedules_due_at ON schedules(due_at);",
    // 5: model calls outside wakes, such as the wake filter's, so the
    // day's spend counts them.
    "CREATE TABLE helper_calls (
         id INTEGER PRIMARY KEY,
         at INTEGER NOT NULL,
         purpose TEXT NOT NULL,
         cost_usd REAL NOT NULL
     );
     CREATE INDEX helper_calls_at ON helper_calls(at);",
    // 6: workers the agent spawned, and the paths each one holds. A lock
    // counts only until its lease runs out, so a crash can't hold a path
    // for ever.
    "CREATE TABLE workers (
         id INTEGER PRIMARY KEY,
         kind TEXT NOT NULL,
         repo TEXT NOT NULL,
         base TEXT NOT NULL,
         branch TEXT NOT NULL,
         worktree TEXT NOT NULL,
         brief TEXT NOT NULL,
         state TEXT NOT NULL CHECK (state IN ('running', 'finished', 'failed', 'stopped')),
         started_at INTEGER NOT NULL,
         ended_at INTEGER,
         summary TEXT
     );
     CREATE TABLE path_locks (
         worker_id INTEGER NOT NULL REFERENCES workers(id) ON DELETE CASCADE,
         repo TEXT NOT NULL,
         path TEXT NOT NULL,
         lease_until INTEGER NOT NULL,
         PRIMARY KEY (worker_id, path)
     );
     CREATE INDEX path_locks_repo ON path_locks(repo, lease_until);",
    // 7: what a worker may change, kept after it ends so it can be steered
    // and take the same paths again; its CLI session; how many turns it has
    // had.
    "ALTER TABLE workers ADD COLUMN paths TEXT NOT NULL DEFAULT '[]';
     ALTER TABLE workers ADD COLUMN session TEXT;
     ALTER TABLE workers ADD COLUMN turns INTEGER NOT NULL DEFAULT 1;
     UPDATE workers SET paths = (
         SELECT json_group_array(path) FROM path_locks WHERE worker_id = workers.id
     ) WHERE EXISTS (SELECT 1 FROM path_locks WHERE worker_id = workers.id);",
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
