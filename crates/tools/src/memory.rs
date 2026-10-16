//! The agent's own memory: `MEMORY.md` (its capped index), `USER.md` (its
//! owner and how they work), the handoff note for tomorrow, and a journal
//! with one file per day. All Markdown, with every change kept in git so it
//! can be undone (ARCHITECTURE.md, "Memory").
//!
//! The git repository lives outside the folder, where the runtime keeps its
//! state. Only the runtime writes its config, so nothing the agent can edit
//! (a filter, a hook, an fsmonitor command) runs when a change is saved.
//!
//! The folder lives in the agent's work area, so the policy executor's
//! path walls apply to every read and write like any other file. Files are
//! opened without following a symlink at the last step: the walls may judge
//! only the journal folder, and a link planted in a restored or edited repo
//! mustn't lead outside it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_providers::ToolSpec;
use serde_json::{Value, json};
use time::{OffsetDateTime, UtcOffset};

use crate::tool::{Effect, Tool, ToolContext, ToolError, ToolOutput};

/// The files the agent rewrites whole, with their caps in characters. The
/// caps keep the memory snapshot, which starts every request, small.
const FILES: [(&str, &str, usize); 3] = [
    ("memory", "MEMORY.md", 6_000),
    ("user", "USER.md", 3_000),
    ("handoff", "HANDOFF.md", 4_000),
];

/// One journal entry's limit.
pub const MAX_ENTRY_CHARS: usize = 4_000;

/// How much of a journal day `memory_read` shows.
const MAX_READ_CHARS: usize = 40_000;

/// The memory folder, shared by the memory tools.
#[derive(Debug)]
pub struct MemoryFiles {
    dir: PathBuf,
    /// Where `dir` really is, fixed when it's opened.
    real: PathBuf,
    /// The git repository that keeps the history, if there is one.
    history: Option<PathBuf>,
    /// The owner's offset, so journal days match theirs.
    offset: UtcOffset,
    lock: Mutex<()>,
}

impl MemoryFiles {
    /// Opens the folder, creating it if needed, with its history kept in
    /// the git repository at `history`, which is created if needed. Without
    /// git on the machine the files still work, without history.
    ///
    /// The folder itself can't be a symlink, so it can't lead somewhere
    /// else.
    pub fn open(
        dir: impl Into<PathBuf>,
        history: Option<PathBuf>,
        offset: UtcOffset,
    ) -> std::io::Result<Self> {
        let dir = dir.into();
        if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(std::io::Error::other("the memory folder is a symlink"));
        }
        std::fs::create_dir_all(dir.join("journal"))?;
        let real = dir.canonicalize()?;
        let history = history.filter(|git_dir| match std::fs::symlink_metadata(git_dir) {
            Ok(meta) => meta.is_dir(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                git(git_dir, &real, &["init", "--quiet"]).is_ok_and(|ok| ok)
            }
            Err(_) => false,
        });
        Ok(Self {
            dir,
            real,
            history,
            offset,
            lock: Mutex::new(()),
        })
    }

    /// The folder, as long as it's still where it was opened: a folder
    /// swapped for a link since then is refused.
    fn root(&self) -> std::io::Result<&Path> {
        if self.real.canonicalize()? == self.real {
            Ok(&self.real)
        } else {
            Err(std::io::Error::other("the memory folder has moved"))
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether changes are kept in git.
    pub fn has_history(&self) -> bool {
        self.history.is_some()
    }

    /// `MEMORY.md`, `USER.md` and the handoff note, for the start of a day.
    /// Each is held to its cap, even if it was edited outside the tools.
    pub fn snapshot(&self) -> String {
        let root = self.root().ok();
        FILES
            .iter()
            .map(|(_, name, cap)| {
                let text = root
                    .and_then(|root| read_capped(&root.join(name), *cap).ok())
                    .unwrap_or_default();
                let text = text.trim();
                format!(
                    "## {name}\n{}",
                    if text.is_empty() { "(empty)" } else { text }
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn journal_day(&self, at: OffsetDateTime) -> String {
        let day = at.to_offset(self.offset).date();
        format!(
            "{:04}-{:02}-{:02}",
            day.year(),
            u8::from(day.month()),
            day.day()
        )
    }

    /// Writes `text` to `relative` and commits it, one change at a time.
    fn save(
        &self,
        relative: &str,
        text: &str,
        append: bool,
        message: &str,
    ) -> Result<(), ToolError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let failed = |e: std::io::Error| {
            ToolError::Failed(format!("couldn't save {relative}: {}", e.kind()))
        };
        let root = self.root().map_err(failed)?;
        let path = root.join(relative);
        let mut options = OpenOptions::new();
        if append {
            options.create(true).append(true);
        } else {
            options.create(true).write(true).truncate(true);
        }
        open_file(&path, &mut options)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .map_err(failed)?;
        // A failed commit loses history, not the change, so it doesn't fail
        // the write.
        if let Some(git_dir) = &self.history
            && git(git_dir, root, &["add", "--", relative]).is_ok_and(|ok| ok)
        {
            let _ = git(
                git_dir,
                root,
                &[
                    "-c",
                    "user.name=harness",
                    "-c",
                    "user.email=harness@localhost",
                    "commit",
                    "--quiet",
                    "--no-verify",
                    "-m",
                    message,
                    "--",
                    relative,
                ],
            );
        }
        Ok(())
    }
}

/// Opens `path` without following a symlink at its last step.
fn open_file(path: &Path, options: &mut OpenOptions) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("the file is a symlink"));
    }
    options.open(path)
}

/// The file's text within `limit` characters, saying so if it was cut, or
/// empty if it doesn't exist yet. Only what can be shown is read, however
/// big the file has grown.
fn read_capped(path: &Path, limit: usize) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    match open_file(path, OpenOptions::new().read(true)) {
        // A character is at most 4 bytes, so one more byte than that shows
        // whether there is more.
        Ok(file) => file.take(limit as u64 * 4 + 1).read_to_end(&mut bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(match text.char_indices().nth(limit) {
        None => text.into_owned(),
        Some((at, _)) => format!("{}\n[cut at {limit} characters]", &text[..at]),
    })
}

/// The last `limit` characters of the file, saying so if earlier ones were
/// left out, or empty if it doesn't exist yet. For the journal, where the
/// newest entries matter most.
fn read_tail(path: &Path, limit: usize) -> std::io::Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = match open_file(path, OpenOptions::new().read(true)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(limit as u64 * 4);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 * 4).read_to_end(&mut bytes)?;
    // Start on a whole character.
    let skip = bytes
        .iter()
        .take_while(|b| (0x80..0xC0).contains(*b))
        .count();
    let text = String::from_utf8_lossy(&bytes[skip..]);
    let count = text.chars().count();
    if start == 0 && count <= limit {
        return Ok(text.into_owned());
    }
    let tail: String = text.chars().skip(count.saturating_sub(limit)).collect();
    Ok(format!(
        "[earlier entries left out; the last {limit} characters]\n{tail}"
    ))
}

/// Runs git on the repository `git_dir` for the files in `work_tree`, with
/// an argument array; whether it succeeded. Hooks, fsmonitor and the
/// machine's global config stay out of it.
fn git(git_dir: &Path, work_tree: &Path, args: &[&str]) -> std::io::Result<bool> {
    Command::new("git")
        .arg("-C")
        .arg(work_tree)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .env("GIT_DIR", git_dir)
        .env("GIT_WORK_TREE", work_tree)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
}

/// The memory tools, sharing one folder.
pub fn tools(files: Arc<MemoryFiles>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(MemoryRead(files.clone())),
        Box::new(MemoryWrite(files.clone())),
        Box::new(JournalWrite(files)),
    ]
}

fn file(args: &Value) -> Result<(&'static str, usize), ToolError> {
    let wanted = args.get("file").and_then(Value::as_str).unwrap_or_default();
    FILES
        .iter()
        .find(|(key, _, _)| *key == wanted)
        .map(|(_, name, cap)| (*name, *cap))
        .ok_or_else(|| ToolError::InvalidArguments("file must be memory, user or handoff".into()))
}

/// `YYYY-MM-DD`, checked strictly so it can only name a journal file.
fn journal_date(args: &Value) -> Result<Option<String>, ToolError> {
    let Some(date) = args.get("date").and_then(Value::as_str) else {
        return Ok(None);
    };
    let ok = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    if !ok {
        return Err(ToolError::InvalidArguments(
            "date must be YYYY-MM-DD".into(),
        ));
    }
    Ok(Some(date.to_string()))
}

struct MemoryRead(Arc<MemoryFiles>);
struct MemoryWrite(Arc<MemoryFiles>);
struct JournalWrite(Arc<MemoryFiles>);

#[async_trait]
impl Tool for MemoryRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_read".into(),
            description: "Read your own memory: memory (MEMORY.md, your index), user (USER.md, \
                          your owner and how they work), handoff (your note for tomorrow), or \
                          journal (one day's entries; today unless you give a date)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "memory, user, handoff or journal"},
                    "date": {"type": "string", "description": "For the journal: YYYY-MM-DD"}
                },
                "required": ["file"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::ReadPath(self.path(args, None)?)])
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let path = self.path(args, Some(ctx.clock.now()))?;
        let journal = args.get("file").and_then(Value::as_str) == Some("journal");
        let text = if journal {
            read_tail(&path, MAX_READ_CHARS)
        } else {
            read_capped(&path, MAX_READ_CHARS)
        }
        .map_err(|e| ToolError::Failed(format!("couldn't read it: {}", e.kind())))?;
        if text.trim().is_empty() {
            return Ok(ToolOutput::text("(empty)"));
        }
        Ok(ToolOutput::text(text))
    }
}

impl MemoryRead {
    /// The file a call names. Without `now`, today's journal is named by
    /// the folder only, which the walls judge the same way.
    fn path(&self, args: &Value, now: Option<OffsetDateTime>) -> Result<PathBuf, ToolError> {
        let files = &self.0;
        if args.get("file").and_then(Value::as_str) == Some("journal") {
            let day = match (journal_date(args)?, now) {
                (Some(day), _) => day,
                (None, Some(now)) => files.journal_day(now),
                (None, None) => return Ok(files.dir.join("journal")),
            };
            return Ok(files.dir.join("journal").join(format!("{day}.md")));
        }
        if args.get("date").is_some() {
            return Err(ToolError::InvalidArguments(
                "date is only for the journal".into(),
            ));
        }
        Ok(files.dir.join(file(args)?.0))
    }
}

#[async_trait]
impl Tool for MemoryWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_write".into(),
            description: "Replace one of your memory files with new content: memory (MEMORY.md, \
                          up to 6000 characters), user (USER.md, up to 3000) or handoff (up to \
                          4000). Every change is kept in history. What you write here starts \
                          each new day."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "memory, user or handoff"},
                    "content": {"type": "string", "description": "The whole new file, in Markdown"}
                },
                "required": ["file", "content"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        let (name, cap) = file(args)?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let length = content.chars().count();
        if length > cap {
            return Err(ToolError::InvalidArguments(format!(
                "{name} is capped at {cap} characters and this is {length}; keep what matters most"
            )));
        }
        Ok(vec![Effect::WritePath(self.0.dir.join(name))])
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        self.effects(args)?;
        let (name, _) = file(args)?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut text = content.trim_end().to_string();
        text.push('\n');
        self.0.save(name, &text, false, &format!("Update {name}"))?;
        Ok(ToolOutput::text(format!("Saved {name}.")))
    }
}

#[async_trait]
impl Tool for JournalWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "journal_write".into(),
            description: "Add an entry to today's journal: what happened, what you learned, what \
                          a brief got right or wrong. Entries are kept, never rewritten."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "entry": {"type": "string", "description": "The entry, in Markdown"}
                },
                "required": ["entry"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        let entry = args
            .get("entry")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if entry.trim().is_empty() {
            return Err(ToolError::InvalidArguments("entry is empty".into()));
        }
        let length = entry.chars().count();
        if length > MAX_ENTRY_CHARS {
            return Err(ToolError::InvalidArguments(format!(
                "an entry is at most {MAX_ENTRY_CHARS} characters and this is {length}"
            )));
        }
        Ok(vec![Effect::WritePath(self.0.dir.join("journal"))])
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        self.effects(args)?;
        let entry = args
            .get("entry")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let now = ctx.clock.now();
        let local = now.to_offset(self.0.offset);
        let day = self.0.journal_day(now);
        let relative = format!("journal/{day}.md");
        let text = format!(
            "## {:02}:{:02}\n\n{}\n\n",
            local.hour(),
            local.minute(),
            entry.trim()
        );
        self.0
            .save(&relative, &text, true, &format!("Journal {day}"))?;
        Ok(ToolOutput::text(format!("Added to the journal for {day}.")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::FakeClock;
    use time::macros::{datetime, offset};

    fn open() -> (tempfile::TempDir, Arc<MemoryFiles>) {
        let dir = tempfile::tempdir().unwrap();
        let files = MemoryFiles::open(
            dir.path().join("memory"),
            Some(dir.path().join("memory.git")),
            offset!(-7),
        )
        .unwrap();
        (dir, Arc::new(files))
    }

    async fn run(tool: &dyn Tool, args: Value, clock: &FakeClock) -> Result<ToolOutput, ToolError> {
        tool.effects(&args)?;
        tool.run(&args, &ToolContext { clock }).await
    }

    fn commits(git_dir: &Path) -> usize {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn memory_is_written_read_and_kept_in_history() {
        let (_tmp, files) = open();
        let clock = FakeClock::new(datetime!(2026-01-05 03:30 UTC));
        let [read, write, journal] = <[Box<dyn Tool>; 3]>::try_from(tools(files.clone()))
            .ok()
            .unwrap();

        assert_eq!(
            run(read.as_ref(), json!({"file": "memory"}), &clock)
                .await
                .unwrap()
                .content,
            "(empty)"
        );
        run(
            write.as_ref(),
            json!({"file": "memory", "content": "- Mischa likes short PRs."}),
            &clock,
        )
        .await
        .unwrap();
        run(
            write.as_ref(),
            json!({"file": "user", "content": "Mischa, UTC-7."}),
            &clock,
        )
        .await
        .unwrap();
        run(
            journal.as_ref(),
            json!({"entry": "Answered the proxy question."}),
            &clock,
        )
        .await
        .unwrap();

        assert_eq!(
            run(read.as_ref(), json!({"file": "memory"}), &clock)
                .await
                .unwrap()
                .content,
            "- Mischa likes short PRs.\n"
        );
        // 03:30 UTC is the evening before in UTC-7.
        let today = run(read.as_ref(), json!({"file": "journal"}), &clock)
            .await
            .unwrap();
        assert_eq!(
            today.content,
            "## 20:30\n\nAnswered the proxy question.\n\n"
        );
        let by_date = run(
            read.as_ref(),
            json!({"file": "journal", "date": "2026-01-04"}),
            &clock,
        )
        .await
        .unwrap();
        assert_eq!(by_date.content, today.content);

        let snapshot = files.snapshot();
        assert!(snapshot.contains("## MEMORY.md\n- Mischa likes short PRs."));
        assert!(snapshot.contains("## USER.md\nMischa, UTC-7."));
        assert!(snapshot.contains("## HANDOFF.md\n(empty)"));
        assert!(files.has_history(), "git keeps the history");
        assert_eq!(commits(&files.dir().with_extension("git")), 3);
    }

    #[tokio::test]
    async fn bad_calls_change_nothing() {
        let (_tmp, files) = open();
        let clock = FakeClock::new(datetime!(2026-01-05 09:00 UTC));
        let [read, write, journal] = <[Box<dyn Tool>; 3]>::try_from(tools(files.clone()))
            .ok()
            .unwrap();

        let too_long = "x".repeat(3_001);
        assert!(
            run(
                write.as_ref(),
                json!({"file": "user", "content": too_long}),
                &clock
            )
            .await
            .is_err()
        );
        assert!(
            run(
                write.as_ref(),
                json!({"file": "../../.ssh/config", "content": "x"}),
                &clock
            )
            .await
            .is_err()
        );
        assert!(
            run(
                write.as_ref(),
                json!({"file": "journal", "content": "x"}),
                &clock
            )
            .await
            .is_err()
        );
        assert!(
            run(
                read.as_ref(),
                json!({"file": "journal", "date": "../../etc/passwd"}),
                &clock
            )
            .await
            .is_err()
        );
        assert!(
            run(
                read.as_ref(),
                json!({"file": "memory", "date": "2026-01-05"}),
                &clock
            )
            .await
            .is_err()
        );
        assert!(
            run(journal.as_ref(), json!({"entry": "  "}), &clock)
                .await
                .is_err()
        );
        assert!(files.snapshot().matches("(empty)").count() == 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn links_planted_in_the_folder_lead_nowhere() {
        let (tmp, files) = open();
        let clock = FakeClock::new(datetime!(2026-01-05 19:00 UTC));
        let [read, write, journal] = <[Box<dyn Tool>; 3]>::try_from(tools(files.clone()))
            .ok()
            .unwrap();
        let secret = tmp.path().join("secret.txt");
        std::fs::write(&secret, "outside").unwrap();
        let link =
            |name: &str| std::os::unix::fs::symlink(&secret, files.dir().join(name)).unwrap();
        link("journal/2026-01-05.md");
        link("USER.md");

        assert!(
            run(read.as_ref(), json!({"file": "journal"}), &clock)
                .await
                .is_err()
        );
        assert!(
            run(journal.as_ref(), json!({"entry": "hello"}), &clock)
                .await
                .is_err()
        );
        assert!(
            run(
                write.as_ref(),
                json!({"file": "user", "content": "overwritten"}),
                &clock
            )
            .await
            .is_err()
        );
        assert!(!files.snapshot().contains("outside"));
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "outside");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nothing_in_the_folder_steers_git() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&other)
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
        // A planted repo whose config runs a command on `git add`.
        let ran = tmp.path().join("filter-ran");
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(other.join(".git/config"))
            .unwrap();
        writeln!(config, "[filter \"x\"]\n\tclean = touch {}", ran.display()).unwrap();
        let dir = tmp.path().join("memory");
        std::fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(other.join(".git"), dir.join(".git")).unwrap();
        std::fs::write(dir.join(".gitattributes"), "* filter=x\n").unwrap();

        let history = tmp.path().join("memory.git");
        let files = MemoryFiles::open(&dir, Some(history.clone()), offset!(-7)).unwrap();
        let files = Arc::new(files);
        let clock = FakeClock::new(datetime!(2026-01-05 19:00 UTC));
        let write = tools(files).remove(1);
        run(
            write.as_ref(),
            json!({"file": "memory", "content": "kept here"}),
            &clock,
        )
        .await
        .unwrap();
        assert_eq!(commits(&history), 1);
        assert_eq!(
            commits(&other.join(".git")),
            0,
            "the other repo is untouched"
        );
        assert!(!ran.exists(), "the planted filter never ran");
    }

    #[cfg(unix)]
    #[test]
    fn the_folder_cant_be_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("MEMORY.md"), "outside").unwrap();
        let link = tmp.path().join("linked");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(MemoryFiles::open(&link, None, offset!(-7)).is_err());

        // Nor can it be swapped for one after it's opened.
        let dir = tmp.path().join("memory");
        let files = MemoryFiles::open(&dir, None, offset!(-7)).unwrap();
        std::fs::rename(&dir, tmp.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();
        assert!(!files.snapshot().contains("outside"));
        assert!(files.save("USER.md", "x", false, "x").is_err());
        assert!(!outside.join("USER.md").exists());
    }

    #[tokio::test]
    async fn a_long_journal_day_shows_its_newest_entries() {
        let (_tmp, files) = open();
        let clock = FakeClock::new(datetime!(2026-01-05 19:00 UTC));
        let [read, _, journal] = <[Box<dyn Tool>; 3]>::try_from(tools(files.clone()))
            .ok()
            .unwrap();
        for i in 0..20 {
            let entry = format!("entry {i} {}", "é".repeat(3_000));
            run(journal.as_ref(), json!({"entry": entry}), &clock)
                .await
                .unwrap();
        }
        let shown = run(read.as_ref(), json!({"file": "journal"}), &clock)
            .await
            .unwrap()
            .content;
        assert!(shown.starts_with("[earlier entries left out"));
        assert!(shown.contains("entry 19 "));
        assert!(!shown.contains("entry 0 "));
        assert!(shown.chars().count() < MAX_READ_CHARS + 100);
    }

    #[test]
    fn the_snapshot_keeps_to_the_caps() {
        let (_tmp, files) = open();
        std::fs::write(files.dir().join("MEMORY.md"), "x".repeat(50_000)).unwrap();
        let snapshot = files.snapshot();
        assert!(snapshot.contains("[cut at 6000 characters]"));
        assert!(snapshot.chars().count() < 6_200);
    }

    #[test]
    fn effects_name_the_files_for_the_walls() {
        let (_tmp, files) = open();
        let [read, write, journal] = <[Box<dyn Tool>; 3]>::try_from(tools(files.clone()))
            .ok()
            .unwrap();
        let dir = files.dir();
        assert_eq!(
            write
                .effects(&json!({"file": "handoff", "content": "x"}))
                .unwrap(),
            vec![Effect::WritePath(dir.join("HANDOFF.md"))]
        );
        assert_eq!(
            journal.effects(&json!({"entry": "x"})).unwrap(),
            vec![Effect::WritePath(dir.join("journal"))]
        );
        assert_eq!(
            read.effects(&json!({"file": "journal", "date": "2026-01-02"}))
                .unwrap(),
            vec![Effect::ReadPath(dir.join("journal/2026-01-02.md"))]
        );
    }
}
