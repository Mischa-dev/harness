//! The workers the agent has spawned, and the paths each one holds.
//!
//! Two workers in one repo can't hold overlapping paths (ARCHITECTURE.md,
//! "Workers"). A worker takes its locks when it starts, all or none, and
//! gives them back when it ends. Each lock has a lease the runtime renews
//! while the worker runs; a lock whose lease has run out no longer counts,
//! so a crash can't hold a path for ever.
//!
//! A worker that ended can be steered: it runs again in the same worktree,
//! taking the same paths again, if nobody else holds them by then.
//!
//! The book is its own handle on the state file, like the schedule, so the
//! worker tools and the wake loop can share it.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use time::{Duration, OffsetDateTime};

use crate::{Result, StoreError, open_file, prepare, timestamp};

/// A path inside a repository that a worker may change: a file or a
/// directory, relative to the repository's root, with `/` between parts.
/// The whole repository is the empty path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LockPath(String);

impl LockPath {
    /// The whole repository.
    pub fn whole_repo() -> Self {
        Self(String::new())
    }

    /// Reads a repo-relative path. `.` or an empty path is the whole
    /// repository; a trailing `/` is dropped. Absolute paths, `..`, `.` in
    /// the middle, empty parts and backslashes are refused, so two spellings
    /// can't name one place.
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.strip_suffix('/').unwrap_or(text);
        if trimmed.is_empty() || trimmed == "." {
            return Some(Self::whole_repo());
        }
        if trimmed.len() > 500 || trimmed.contains(['\\', '\0']) || trimmed.starts_with('/') {
            return None;
        }
        let ok = trimmed
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
        ok.then(|| Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `other` is this path or inside it, part by part: `src`
    /// contains `src/main.rs`, but not `src2`.
    pub fn contains(&self, other: &Self) -> bool {
        self.0.is_empty()
            || other.0 == self.0
            || other
                .0
                .strip_prefix(&self.0)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Whether one path contains the other.
    pub fn overlaps(&self, other: &Self) -> bool {
        self.contains(other) || other.contains(self)
    }
}

impl std::fmt::Display for LockPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("the whole repository")
        } else {
            f.write_str(&self.0)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Running,
    /// Ended by itself, with something to report.
    Finished,
    /// Ended by itself without finishing: a crash, a limit, an error.
    Failed,
    /// Stopped by the agent or the runtime.
    Stopped,
}

impl WorkerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "running" => Ok(Self::Running),
            "finished" => Ok(Self::Finished),
            "failed" => Ok(Self::Failed),
            "stopped" => Ok(Self::Stopped),
            other => Err(StoreError::Corrupt(format!("worker state {other:?}"))),
        }
    }
}

/// What a new worker is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorker {
    /// Which kind of worker, such as `claude_code`.
    pub kind: String,
    /// The repository's name in the agent's config.
    pub repo: String,
    /// The commit or branch the worker started from.
    pub base: String,
    pub branch: String,
    pub worktree: String,
    pub brief: String,
}

/// A worker, as the book has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRecord {
    pub id: i64,
    pub kind: String,
    pub repo: String,
    pub base: String,
    pub branch: String,
    pub worktree: String,
    pub brief: String,
    pub state: WorkerState,
    pub started_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
    /// What it reported when it last ended.
    pub summary: Option<String>,
    /// The paths it may change, which it holds while it runs.
    pub paths: Vec<LockPath>,
    /// The coding CLI's session, if it said, to continue it when steered.
    pub session: Option<String>,
    /// How many times it has run: 1, plus once for each steer.
    pub turns: u32,
}

/// Why an ended worker couldn't run again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeRefusal {
    Missing,
    Running,
    /// It was recorded before workers kept their paths.
    NoPaths,
    Conflict(LockConflict),
}

impl std::fmt::Display for ResumeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("there's no such worker"),
            Self::Running => f.write_str("it's still running"),
            Self::NoPaths => f.write_str("it has no paths on record"),
            Self::Conflict(conflict) => conflict.fmt(f),
        }
    }
}

/// Why a worker couldn't start: another one holds an overlapping path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockConflict {
    pub worker_id: i64,
    pub held: LockPath,
    pub wanted: LockPath,
}

impl std::fmt::Display for LockConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "worker #{} holds {}, which overlaps {}",
            self.worker_id, self.held, self.wanted
        )
    }
}

pub struct WorkerBook {
    conn: Mutex<Connection>,
}

impl WorkerBook {
    /// Opens the book in the state file at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: Mutex::new(prepare(open_file(path)?)?),
        })
    }

    /// A book of its own in memory, for tests and simulations.
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

    /// Records a new running worker holding `paths` until `now + lease`, if
    /// no other worker in the same repository holds an overlapping path.
    /// All or nothing, in one transaction.
    pub fn start(
        &self,
        worker: &NewWorker,
        paths: &[LockPath],
        lease: Duration,
        now: OffsetDateTime,
    ) -> Result<std::result::Result<WorkerRecord, LockConflict>> {
        let mut paths = paths.to_vec();
        paths.sort();
        paths.dedup();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let Some(conflict) = conflict(&tx, &worker.repo, &paths, now)? {
            return Ok(Err(conflict));
        }
        let names: Vec<&str> = paths.iter().map(LockPath::as_str).collect();
        tx.execute(
            "INSERT INTO workers (kind, repo, base, branch, worktree, brief, state, started_at, paths)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, ?8)",
            params![
                worker.kind,
                worker.repo,
                worker.base,
                worker.branch,
                worker.worktree,
                worker.brief,
                now.unix_timestamp(),
                serde_json::to_string(&names).map_err(|e| StoreError::Corrupt(e.to_string()))?
            ],
        )?;
        let id = tx.last_insert_rowid();
        take_locks(
            &tx,
            id,
            &worker.repo,
            &paths,
            (now + lease).unix_timestamp(),
        )?;
        tx.commit()?;
        drop(conn);
        let record = self
            .get(id)?
            .ok_or_else(|| StoreError::Corrupt(format!("worker {id} vanished")))?;
        Ok(Ok(record))
    }

    /// Extends a running worker's locks to `now + lease`. Returns `false`,
    /// extending nothing, when it has ended or has lost a path: its lease
    /// ran out and another worker took an overlapping one. The runtime
    /// stops a worker that lost its paths.
    pub fn renew(&self, id: i64, lease: Duration, now: OffsetDateTime) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let repo: Option<String> = tx
            .query_row(
                "SELECT repo FROM workers WHERE id = ?1 AND state = 'running'",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(repo) = repo else {
            return Ok(false);
        };
        let mine: Vec<LockPath> = {
            let mut select = tx.prepare("SELECT path FROM path_locks WHERE worker_id = ?1")?;
            select
                .query_map(params![id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .iter()
                .filter_map(|p| LockPath::parse(p))
                .collect()
        };
        let others = held_locks(&tx, &repo, now)?;
        let lost = mine.iter().any(|path| {
            others
                .iter()
                .any(|(other, held)| *other != id && held.overlaps(path))
        });
        if lost || mine.is_empty() {
            return Ok(false);
        }
        tx.execute(
            "UPDATE path_locks SET lease_until = ?1 WHERE worker_id = ?2",
            params![(now + lease).unix_timestamp(), id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Ends a running worker and gives back its locks. Returns `false` if it
    /// had already ended, and changes nothing then.
    pub fn end(
        &self,
        id: i64,
        state: WorkerState,
        summary: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        if state == WorkerState::Running {
            return Err(StoreError::Corrupt("a worker can't end as running".into()));
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE workers SET state = ?1, summary = ?2, ended_at = ?3
             WHERE id = ?4 AND state = 'running'",
            params![state.as_str(), summary, now.unix_timestamp(), id],
        )?;
        if changed > 0 {
            tx.execute("DELETE FROM path_locks WHERE worker_id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Records the coding CLI's session for a worker, to continue it later.
    pub fn set_session(&self, id: i64, session: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE workers SET session = ?1 WHERE id = ?2",
            params![session, id],
        )?;
        Ok(())
    }

    /// Runs an ended worker again: it takes its paths again until
    /// `now + lease`, if no other worker holds an overlapping one, and
    /// counts another turn. All or nothing, like starting.
    pub fn resume(
        &self,
        id: i64,
        lease: Duration,
        now: OffsetDateTime,
    ) -> Result<std::result::Result<WorkerRecord, ResumeRefusal>> {
        let Some(worker) = self.get(id)? else {
            return Ok(Err(ResumeRefusal::Missing));
        };
        if worker.state == WorkerState::Running {
            return Ok(Err(ResumeRefusal::Running));
        }
        if worker.paths.is_empty() {
            return Ok(Err(ResumeRefusal::NoPaths));
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let Some(conflict) = conflict(&tx, &worker.repo, &worker.paths, now)? {
            return Ok(Err(ResumeRefusal::Conflict(conflict)));
        }
        let changed = tx.execute(
            "UPDATE workers SET state = 'running', ended_at = NULL, turns = turns + 1
             WHERE id = ?1 AND state != 'running'",
            params![id],
        )?;
        if changed == 0 {
            return Ok(Err(ResumeRefusal::Running));
        }
        take_locks(
            &tx,
            id,
            &worker.repo,
            &worker.paths,
            (now + lease).unix_timestamp(),
        )?;
        tx.commit()?;
        drop(conn);
        let record = self
            .get(id)?
            .ok_or_else(|| StoreError::Corrupt(format!("worker {id} vanished")))?;
        Ok(Ok(record))
    }

    pub fn get(&self, id: i64) -> Result<Option<WorkerRecord>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM workers WHERE id = ?1"),
                params![id],
                raw_row,
            )
            .optional()?;
        row.map(record).transpose()
    }

    /// Running workers, oldest first.
    pub fn running(&self) -> Result<Vec<WorkerRecord>> {
        self.select("WHERE state = 'running' ORDER BY id", [])
    }

    /// The most recent workers, running or not, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<WorkerRecord>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.select("ORDER BY id DESC LIMIT ?1", [limit])
    }

    fn select<P: rusqlite::Params>(&self, tail: &str, params: P) -> Result<Vec<WorkerRecord>> {
        let conn = self.conn();
        let rows = {
            let mut select = conn.prepare(&format!("SELECT {COLUMNS} FROM workers {tail}"))?;
            select
                .query_map(params, raw_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        rows.into_iter().map(record).collect()
    }
}

const COLUMNS: &str = "id, kind, repo, base, branch, worktree, brief, state, started_at, \
                       ended_at, summary, paths, session, turns";

type RawRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    Option<String>,
    String,
    Option<String>,
    i64,
);

fn raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn record(row: RawRow) -> Result<WorkerRecord> {
    let (id, kind, repo, base, branch, worktree, brief, state, started, ended, summary, paths) = (
        row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11,
    );
    let paths: Vec<String> = serde_json::from_str(&paths)
        .map_err(|e| StoreError::Corrupt(format!("worker {id} paths: {e}")))?;
    Ok(WorkerRecord {
        id,
        kind,
        repo,
        base,
        branch,
        worktree,
        brief,
        state: WorkerState::parse(&state)?,
        started_at: timestamp(started)?,
        ended_at: ended.map(timestamp).transpose()?,
        summary,
        paths: paths
            .iter()
            .map(|p| {
                LockPath::parse(p).ok_or_else(|| StoreError::Corrupt(format!("lock path {p:?}")))
            })
            .collect::<Result<_>>()?,
        session: row.12,
        turns: u32::try_from(row.13)
            .map_err(|_| StoreError::Corrupt(format!("worker {id} turns")))?,
    })
}

/// The first held lock in `repo` that overlaps one of `paths`.
fn conflict(
    conn: &Connection,
    repo: &str,
    paths: &[LockPath],
    now: OffsetDateTime,
) -> Result<Option<LockConflict>> {
    let held = held_locks(conn, repo, now)?;
    Ok(paths.iter().find_map(|wanted| {
        held.iter()
            .find(|(_, held)| held.overlaps(wanted))
            .map(|(worker_id, held)| LockConflict {
                worker_id: *worker_id,
                held: held.clone(),
                wanted: wanted.clone(),
            })
    }))
}

fn take_locks(
    conn: &Connection,
    id: i64,
    repo: &str,
    paths: &[LockPath],
    until: i64,
) -> Result<()> {
    for path in paths {
        conn.execute(
            "INSERT INTO path_locks (worker_id, repo, path, lease_until) VALUES (?1, ?2, ?3, ?4)",
            params![id, repo, path.as_str(), until],
        )?;
    }
    Ok(())
}

/// Locks in `repo` whose leases haven't run out at `now`, by running
/// workers.
fn held_locks(conn: &Connection, repo: &str, now: OffsetDateTime) -> Result<Vec<(i64, LockPath)>> {
    let mut select = conn.prepare(
        "SELECT l.worker_id, l.path FROM path_locks l JOIN workers w ON w.id = l.worker_id
         WHERE l.repo = ?1 AND l.lease_until > ?2 AND w.state = 'running'",
    )?;
    let rows = select
        .query_map(params![repo, now.unix_timestamp()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, path)| {
            LockPath::parse(&path)
                .map(|path| (id, path))
                .ok_or_else(|| StoreError::Corrupt(format!("lock path {path:?}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-16 22:00 UTC);
    const LEASE: Duration = Duration::minutes(10);

    fn path(text: &str) -> LockPath {
        LockPath::parse(text).unwrap()
    }

    fn worker(repo: &str) -> NewWorker {
        NewWorker {
            kind: "claude_code".into(),
            repo: repo.into(),
            base: "abc123".into(),
            branch: "harness/worker".into(),
            worktree: "/tmp/w".into(),
            brief: "Fix the flaky test.".into(),
        }
    }

    fn start(
        book: &WorkerBook,
        repo: &str,
        paths: &[&str],
        now: OffsetDateTime,
    ) -> std::result::Result<WorkerRecord, LockConflict> {
        let paths: Vec<_> = paths.iter().map(|p| path(p)).collect();
        book.start(&worker(repo), &paths, LEASE, now).unwrap()
    }

    #[test]
    fn paths_have_one_spelling() {
        assert_eq!(path("src/"), path("src"));
        assert_eq!(path("."), LockPath::whole_repo());
        assert_eq!(path(""), LockPath::whole_repo());
        for bad in [
            "/etc", "../x", "src/../x", "src/./x", "./src", "src//x", "a\\b", "a\0b",
        ] {
            assert_eq!(LockPath::parse(bad), None, "{bad:?}");
        }
        assert_eq!(LockPath::parse(&"a".repeat(501)), None);
    }

    #[test]
    fn overlap_is_by_whole_parts() {
        assert!(path("src").overlaps(&path("src/main.rs")));
        assert!(path("src/main.rs").overlaps(&path("src")));
        assert!(path("src").overlaps(&path("src")));
        assert!(LockPath::whole_repo().overlaps(&path("docs/a.md")));
        assert!(path("docs/a.md").overlaps(&LockPath::whole_repo()));
        assert!(!path("src").overlaps(&path("src2")));
        assert!(!path("src2/x").overlaps(&path("src")));
        assert!(!path("src/a.rs").overlaps(&path("src/b.rs")));
        assert!(path("src").contains(&path("src/a.rs")));
        assert!(!path("src/a.rs").contains(&path("src")));
    }

    #[test]
    fn overlapping_paths_in_one_repo_are_refused_and_nothing_is_taken() {
        let book = WorkerBook::open_in_memory().unwrap();
        let first = start(&book, "harness", &["crates/store"], NOW).unwrap();
        assert_eq!(first.state, WorkerState::Running);
        assert_eq!(first.paths, vec![path("crates/store")]);

        let conflict =
            start(&book, "harness", &["docs", "crates/store/src/lib.rs"], NOW).unwrap_err();
        assert_eq!(
            conflict,
            LockConflict {
                worker_id: first.id,
                held: path("crates/store"),
                wanted: path("crates/store/src/lib.rs"),
            }
        );
        // The refused worker took nothing, not even `docs`.
        assert_eq!(book.running().unwrap().len(), 1);
        assert!(start(&book, "harness", &["docs"], NOW).is_ok());
    }

    #[test]
    fn the_whole_repo_conflicts_with_everything_in_it_but_not_other_repos() {
        let book = WorkerBook::open_in_memory().unwrap();
        start(&book, "harness", &["."], NOW).unwrap();
        assert!(start(&book, "harness", &["README.md"], NOW).is_err());
        assert!(start(&book, "workspace", &["README.md"], NOW).is_ok());
    }

    #[test]
    fn disjoint_paths_can_be_held_side_by_side() {
        let book = WorkerBook::open_in_memory().unwrap();
        start(&book, "harness", &["crates/store"], NOW).unwrap();
        start(&book, "harness", &["crates/tools", "crates/store2"], NOW).unwrap();
        assert_eq!(book.running().unwrap().len(), 2);
    }

    #[test]
    fn a_worker_cant_conflict_with_itself() {
        let book = WorkerBook::open_in_memory().unwrap();
        let record = start(&book, "harness", &["src", "src/", "src/main.rs"], NOW).unwrap();
        assert_eq!(record.paths, vec![path("src"), path("src/main.rs")]);
    }

    #[test]
    fn a_lease_that_ran_out_no_longer_holds_the_path() {
        let book = WorkerBook::open_in_memory().unwrap();
        let first = start(&book, "harness", &["src"], NOW).unwrap();
        // At exactly the lease's end, the lock is gone.
        assert!(start(&book, "harness", &["src"], NOW + LEASE - Duration::SECOND).is_err());
        let later = NOW + LEASE;
        assert!(start(&book, "harness", &["src/lib.rs"], later).is_ok());
        // The first worker lost its path: renewing says so and takes
        // nothing back, so two workers never hold it.
        assert!(!book.renew(first.id, LEASE, later).unwrap());
        assert!(start(&book, "harness", &["src"], later).is_err());
    }

    #[test]
    fn renewing_keeps_the_lock() {
        let book = WorkerBook::open_in_memory().unwrap();
        let first = start(&book, "harness", &["src"], NOW).unwrap();
        let later = NOW + LEASE - Duration::SECOND;
        assert!(book.renew(first.id, LEASE, later).unwrap());
        assert!(start(&book, "harness", &["src"], NOW + LEASE + Duration::MINUTE).is_err());
        // Renewing after the lease ran out works while nobody took the path.
        let late = later + LEASE * 2;
        assert!(book.renew(first.id, LEASE, late).unwrap());
        assert!(start(&book, "harness", &["src"], late).is_err());
    }

    #[test]
    fn ending_a_worker_gives_its_paths_back_once() {
        let book = WorkerBook::open_in_memory().unwrap();
        let first = start(&book, "harness", &["src"], NOW).unwrap();
        let later = NOW + Duration::minutes(3);
        assert!(
            book.end(first.id, WorkerState::Finished, "diff ready", later)
                .unwrap()
        );
        assert!(
            !book
                .end(first.id, WorkerState::Stopped, "again", later)
                .unwrap()
        );
        assert!(
            !book.renew(first.id, LEASE, later).unwrap(),
            "an ended worker can't renew"
        );

        let ended = book.get(first.id).unwrap().unwrap();
        assert_eq!(ended.state, WorkerState::Finished);
        assert_eq!(ended.summary.as_deref(), Some("diff ready"));
        assert_eq!(ended.ended_at, Some(later));
        // It keeps its paths on record, but holds none.
        assert_eq!(ended.paths, vec![path("src")]);
        assert!(book.running().unwrap().is_empty());
        assert!(start(&book, "harness", &["src"], later).is_ok());
        assert!(matches!(
            book.end(first.id, WorkerState::Running, "", later),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn locks_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.db");
        let first = {
            let book = WorkerBook::open(&file).unwrap();
            start(&book, "harness", &["src"], NOW).unwrap()
        };
        let book = WorkerBook::open(&file).unwrap();
        assert_eq!(book.running().unwrap(), vec![first.clone()]);
        assert_eq!(
            start(&book, "harness", &["src/lib.rs"], NOW)
                .unwrap_err()
                .worker_id,
            first.id
        );
    }

    #[test]
    fn recent_lists_newest_first() {
        let book = WorkerBook::open_in_memory().unwrap();
        let a = start(&book, "harness", &["a"], NOW).unwrap();
        let b = start(&book, "harness", &["b"], NOW).unwrap();
        book.end(a.id, WorkerState::Failed, "crashed", NOW).unwrap();
        let recent = book.recent(5).unwrap();
        assert_eq!(
            recent.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![b.id, a.id]
        );
        assert_eq!(recent[1].state, WorkerState::Failed);
        assert_eq!(book.recent(1).unwrap().len(), 1);
    }

    #[test]
    fn an_ended_worker_resumes_with_its_paths_unless_someone_took_them() {
        let book = WorkerBook::open_in_memory().unwrap();
        let first = start(&book, "harness", &["src", "docs"], NOW).unwrap();
        assert_eq!(first.turns, 1);
        assert_eq!(first.session, None);
        assert_eq!(
            book.resume(first.id, LEASE, NOW).unwrap(),
            Err(ResumeRefusal::Running)
        );
        book.set_session(first.id, "s-1").unwrap();
        book.end(first.id, WorkerState::Finished, "done", NOW)
            .unwrap();

        // Someone took part of its paths meanwhile: it can't run, and takes
        // nothing.
        let other = start(&book, "harness", &["docs/a.md"], NOW).unwrap();
        assert_eq!(
            book.resume(first.id, LEASE, NOW).unwrap(),
            Err(ResumeRefusal::Conflict(LockConflict {
                worker_id: other.id,
                held: path("docs/a.md"),
                wanted: path("docs"),
            }))
        );
        assert_eq!(book.running().unwrap().len(), 1);
        // Its own paths weren't taken by the refusal.
        let src = start(&book, "harness", &["src"], NOW).unwrap();
        book.end(src.id, WorkerState::Stopped, "", NOW).unwrap();

        book.end(other.id, WorkerState::Finished, "", NOW).unwrap();
        let again = book.resume(first.id, LEASE, NOW).unwrap().unwrap();
        assert_eq!(again.state, WorkerState::Running);
        assert_eq!(again.turns, 2);
        assert_eq!(again.ended_at, None);
        assert_eq!(again.session.as_deref(), Some("s-1"));
        assert_eq!(again.paths, vec![path("docs"), path("src")]);
        // It holds them again, and renews them.
        assert!(start(&book, "harness", &["src/lib.rs"], NOW).is_err());
        assert!(book.renew(first.id, LEASE, NOW).unwrap());
        assert_eq!(
            book.resume(9_999, LEASE, NOW).unwrap(),
            Err(ResumeRefusal::Missing)
        );
    }

    #[test]
    fn workers_from_before_paths_were_kept_get_them_from_their_locks() {
        let conn = Connection::open_in_memory().unwrap();
        // Run the migrations up to 6, add a running and an ended worker,
        // then the rest.
        for (i, sql) in crate::schema::MIGRATIONS.iter().take(6).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO workers (id, kind, repo, base, branch, worktree, brief, state, started_at)
             VALUES (1, 'codex', 'h', 'b', 'x', '/w', 'run', 'running', 0),
                    (2, 'codex', 'h', 'b', 'y', '/w2', 'end', 'finished', 0);
             INSERT INTO path_locks VALUES (1, 'h', 'src', 100), (1, 'h', 'docs', 100);",
        )
        .unwrap();
        let book = WorkerBook {
            conn: Mutex::new(prepare(conn).unwrap()),
        };
        let mut running = book.get(1).unwrap().unwrap().paths;
        running.sort();
        assert_eq!(running, vec![path("docs"), path("src")]);
        assert_eq!(book.get(2).unwrap().unwrap().paths, vec![]);
        assert_eq!(
            book.resume(2, LEASE, NOW).unwrap(),
            Err(ResumeRefusal::NoPaths)
        );
    }
}
