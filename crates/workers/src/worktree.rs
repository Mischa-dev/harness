//! A worker's git worktree: a checkout of the repository on a branch of its
//! own, so its changes stay apart from the agent's and other workers' until
//! the agent judges them.
//!
//! Git runs with argument arrays, never through a shell, and with hooks off,
//! so nothing in a repository's config runs on the agent's behalf.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use harness_store::LockPath;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("couldn't run git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git {command} failed: {stderr}")]
    Failed { command: String, stderr: String },
    #[error("{0}")]
    Invalid(String),
}

/// One changed path, compared with where the worker started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `A`dded, `M`odified, `D`eleted, or another git status letter.
    pub status: char,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The repository the worktree belongs to.
    pub repo: PathBuf,
    pub dir: PathBuf,
    pub branch: String,
    /// The commit the worker started from.
    pub base: String,
}

impl Worktree {
    /// Checks out `base` on a new branch `branch` in `dir`, which mustn't
    /// exist yet.
    pub fn create(repo: &Path, dir: &Path, branch: &str, base: &str) -> Result<Self, GitError> {
        if dir.exists() {
            return Err(GitError::Invalid(format!(
                "{} already exists",
                dir.display()
            )));
        }
        check_ref(repo, branch)?;
        if base.is_empty() || base.starts_with('-') || base.contains(char::is_whitespace) {
            return Err(GitError::Invalid(format!(
                "{base:?} isn't a commit or branch"
            )));
        }
        let resolved = git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{base}^{{commit}}"),
            ],
        )
        .map_err(|_| GitError::Invalid(format!("{base:?} isn't a commit or branch here")))?;
        let base = String::from_utf8_lossy(&resolved).trim().to_string();
        let dir_arg = dir
            .to_str()
            .ok_or_else(|| GitError::Invalid("the worktree path isn't UTF-8".into()))?;
        git(
            repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--no-track",
                "-b",
                branch,
                dir_arg,
                &base,
            ],
        )?;
        Ok(Self {
            repo: repo.to_path_buf(),
            dir: dir.to_path_buf(),
            branch: branch.to_string(),
            base,
        })
    }

    /// Everything the worker changed since it started, committed or not.
    /// Stages what's in the worktree to see untracked files too.
    pub fn changes(&self) -> Result<Vec<Change>, GitError> {
        git(&self.dir, &["add", "--all"])?;
        let out = git(
            &self.dir,
            &[
                "diff",
                "--cached",
                "--no-renames",
                "--name-status",
                "-z",
                &self.base,
            ],
        )?;
        let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
        let mut changes = vec![];
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            changes.push(Change {
                status: status.first().map_or('?', |b| char::from(*b)),
                path: String::from_utf8_lossy(path).into_owned(),
            });
        }
        Ok(changes)
    }

    /// The diff since the worker started, with a summary first, cut to
    /// about `max_bytes`.
    pub fn diff(&self, max_bytes: usize) -> Result<String, GitError> {
        git(&self.dir, &["add", "--all"])?;
        let stat = git(
            &self.dir,
            &["diff", "--cached", "--no-renames", "--stat", &self.base],
        )?;
        let patch = git(&self.dir, &["diff", "--cached", "--no-renames", &self.base])?;
        let mut text = String::from_utf8_lossy(&stat).into_owned();
        text.push('\n');
        let patch = String::from_utf8_lossy(&patch);
        let room = max_bytes.saturating_sub(text.len());
        if patch.len() <= room {
            text.push_str(&patch);
        } else {
            let mut cut = room;
            while !patch.is_char_boundary(cut) {
                cut -= 1;
            }
            text.push_str(&patch[..cut]);
            text.push_str(&format!(
                "\n[diff cut here: {} more bytes; read the worktree for the rest]",
                patch.len() - cut
            ));
        }
        Ok(text)
    }

    /// Removes the worktree and its branch, for work the agent discards.
    pub fn remove(&self) -> Result<(), GitError> {
        let dir = self
            .dir
            .to_str()
            .ok_or_else(|| GitError::Invalid("the worktree path isn't UTF-8".into()))?;
        git(&self.repo, &["worktree", "remove", "--force", dir])?;
        git(&self.repo, &["branch", "--delete", "--force", &self.branch])?;
        Ok(())
    }
}

/// The changes outside the paths a worker locked. A path git reports that
/// can't be read as a lock path counts as outside.
pub fn outside_locks<'a>(changes: &'a [Change], locks: &[LockPath]) -> Vec<&'a Change> {
    changes
        .iter()
        .filter(|change| {
            !LockPath::parse(&change.path)
                .is_some_and(|path| locks.iter().any(|lock| lock.contains(&path)))
        })
        .collect()
}

/// The changes to protected paths: CI, review rules, agent instructions,
/// anything a worker mustn't change to get its work through.
pub fn protected_changes<'a>(changes: &'a [Change], protected: &[LockPath]) -> Vec<&'a Change> {
    changes
        .iter()
        .filter(|change| {
            LockPath::parse(&change.path)
                .is_none_or(|path| protected.iter().any(|p| p.contains(&path)))
        })
        .collect()
}

/// Tests the worker deleted. Weakening a test to pass is a wall
/// (ARCHITECTURE.md, "Walls"); deleting one is the plainest way.
pub fn deleted_tests(changes: &[Change]) -> Vec<&Change> {
    changes
        .iter()
        .filter(|change| change.status == 'D' && is_test(&change.path))
        .collect()
}

/// Whether a path looks like a test by the usual conventions.
pub fn is_test(path: &str) -> bool {
    let mut parts = path.split('/');
    let file = parts.next_back().unwrap_or_default();
    let in_test_dir = path.split('/').rev().skip(1).any(|dir| {
        matches!(
            dir,
            "test" | "tests" | "__tests__" | "spec" | "specs" | "testdata"
        )
    });
    let stem = file.split('.').next().unwrap_or_default();
    in_test_dir
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || file.contains(".test.")
        || file.contains(".spec.")
}

fn check_ref(repo: &Path, branch: &str) -> Result<(), GitError> {
    let plain = branch
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'));
    if branch.is_empty() || branch.starts_with('-') || !plain {
        return Err(GitError::Invalid(format!("{branch:?} isn't a branch name")));
    }
    git(repo, &["check-ref-format", "--branch", branch])
        .map(|_| ())
        .map_err(|_| GitError::Invalid(format!("{branch:?} isn't a branch name")))
}

/// Runs git in `dir` with hooks and the fsmonitor off and no prompts.
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::Failed {
            command: args.first().copied().unwrap_or_default().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one commit: `src/lib.rs` and `README.md`.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        std::fs::create_dir(path.join("src")).unwrap();
        std::fs::write(path.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
        std::fs::write(path.join("README.md"), "# demo\n").unwrap();
        for args in [
            &["init", "--quiet", "--initial-branch=main"][..],
            &["add", "--all"],
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "first",
            ],
        ] {
            git(path, args).unwrap();
        }
        dir
    }

    fn change(status: char, path: &str) -> Change {
        Change {
            status,
            path: path.into(),
        }
    }

    #[test]
    fn a_worktree_sees_every_change_and_can_be_removed() {
        let repo = repo();
        let place = tempfile::tempdir().unwrap();
        let dir = place.path().join("worker-1");
        let tree = Worktree::create(repo.path(), &dir, "harness/worker-1", "main").unwrap();
        assert_eq!(tree.base.len(), 40, "the base is pinned to a commit");
        assert!(dir.join("src/lib.rs").exists());

        std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> u8 { 2 }\n").unwrap();
        std::fs::write(dir.join("src/new.rs"), "// new\n").unwrap();
        std::fs::remove_file(dir.join("README.md")).unwrap();
        let mut changes = tree.changes().unwrap();
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            changes,
            vec![
                change('D', "README.md"),
                change('M', "src/lib.rs"),
                change('A', "src/new.rs"),
            ]
        );
        let diff = tree.diff(10_000).unwrap();
        assert!(diff.contains("3 files changed"), "{diff}");
        assert!(diff.contains("+pub fn one() -> u8 { 2 }"), "{diff}");

        // The agent's checkout is untouched.
        let main = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
        assert!(main.contains("{ 1 }"));

        tree.remove().unwrap();
        assert!(!dir.exists());
        assert!(
            git(
                repo.path(),
                &["rev-parse", "--verify", "--quiet", "harness/worker-1"]
            )
            .is_err()
        );
    }

    #[test]
    fn a_long_diff_is_cut_and_says_so() {
        let repo = repo();
        let place = tempfile::tempdir().unwrap();
        let tree =
            Worktree::create(repo.path(), &place.path().join("w"), "harness/w", "main").unwrap();
        std::fs::write(tree.dir.join("big.txt"), "é line\n".repeat(2_000)).unwrap();
        let diff = tree.diff(600).unwrap();
        assert!(diff.len() < 700, "{}", diff.len());
        assert!(diff.contains("[diff cut here:"), "{diff}");
    }

    #[test]
    fn bad_names_and_bases_are_refused_before_git_runs() {
        let repo = repo();
        let place = tempfile::tempdir().unwrap();
        let dir = place.path().join("w");
        for branch in ["-b", "a b", "a..b", "x;rm", ""] {
            assert!(
                matches!(
                    Worktree::create(repo.path(), &dir, branch, "main"),
                    Err(GitError::Invalid(_))
                ),
                "{branch:?}"
            );
        }
        for base in ["--output=/tmp/x", "no-such-branch", "", "main main"] {
            assert!(
                matches!(
                    Worktree::create(repo.path(), &dir, "harness/w", base),
                    Err(GitError::Invalid(_))
                ),
                "{base:?}"
            );
        }
        assert!(!dir.exists());
        std::fs::create_dir(&dir).unwrap();
        assert!(matches!(
            Worktree::create(repo.path(), &dir, "harness/w", "main"),
            Err(GitError::Invalid(_))
        ));
    }

    #[test]
    fn changes_outside_the_locks_are_found() {
        let locks = [
            LockPath::parse("src").unwrap(),
            LockPath::parse("docs/a.md").unwrap(),
        ];
        let changes = [
            change('M', "src/lib.rs"),
            change('A', "docs/a.md"),
            change('A', "docs/b.md"),
            change('M', "src2/x.rs"),
            change('M', "Cargo.toml"),
        ];
        let outside: Vec<_> = outside_locks(&changes, &locks)
            .iter()
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(outside, ["docs/b.md", "src2/x.rs", "Cargo.toml"]);
        assert!(outside_locks(&changes, &[LockPath::whole_repo()]).is_empty());
    }

    #[test]
    fn protected_paths_and_deleted_tests_are_found() {
        let protected = [
            LockPath::parse(".github").unwrap(),
            LockPath::parse("AGENTS.md").unwrap(),
        ];
        let changes = [
            change('M', ".github/workflows/ci.yml"),
            change('M', "AGENTS.md"),
            change('M', "docs/AGENTS.md"),
            change('M', ".githubx"),
            change('D', "crates/store/tests/run.rs"),
            change('D', "src/lib_test.go"),
            change('D', "web/app.spec.ts"),
            change('M', "tests/run.rs"),
            change('D', "src/testing.rs"),
            change('D', "src/latest.rs"),
        ];
        let touched: Vec<_> = protected_changes(&changes, &protected)
            .iter()
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(touched, [".github/workflows/ci.yml", "AGENTS.md"]);
        let deleted: Vec<_> = deleted_tests(&changes)
            .iter()
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(
            deleted,
            [
                "crates/store/tests/run.rs",
                "src/lib_test.go",
                "web/app.spec.ts"
            ]
        );
        assert!(is_test("test_api.py"));
        assert!(!is_test("contest.rs"));
    }
}
