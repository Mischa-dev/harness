//! Workers: well-defined tasks the agent hands off (ARCHITECTURE.md,
//! "Workers"). Each one works in its own git worktree, on its own branch,
//! and may change only the paths it locked when it started.

pub mod worktree;

pub use worktree::{Change, GitError, Worktree, outside_locks};
