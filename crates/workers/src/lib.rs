//! Workers: well-defined tasks the agent hands off (ARCHITECTURE.md,
//! "Workers"). Each one is a coding CLI working in its own git worktree, on
//! its own branch, that may change only the paths it locked when it
//! started, with no team credentials. It reports back to the agent that
//! spawned it, and nothing else.

pub mod kind;
pub mod runner;
pub mod tools;
pub mod worktree;

pub use kind::{Kind, KindConfig};
pub use runner::{Finished, Request, Workers, WorkersConfig};
pub use worktree::{
    Change, GitError, Worktree, deleted_tests, is_test, outside_locks, protected_changes,
};
