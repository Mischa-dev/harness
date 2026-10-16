//! `read_file` and `search_files`: the agent's look at its own work area.
//!
//! The executor judges the path a call names. Both tools judge again, with
//! the same walls, every path they actually open: the real path behind a
//! read, and each folder and file a search walks into. A search never shows
//! a line from a file a direct read would be refused.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use harness_providers::ToolSpec;
use serde_json::{Value, json};

use crate::policy::{Decision, Policy};
use crate::tool::{Effect, Tool, ToolContext, ToolError, ToolOutput};

/// Lines a read shows when the call doesn't say.
pub const DEFAULT_LINES: usize = 400;
/// The most lines one read shows.
pub const MAX_LINES: usize = 2000;
/// The most characters one read or search shows, all lines together.
pub const MAX_CHARS: usize = 40_000;
/// Longer lines are cut, so one minified file can't fill the context.
pub const MAX_LINE_CHARS: usize = 2000;
/// The most entries a folder listing shows.
pub const MAX_ENTRIES: usize = 500;
/// The most matching lines a search shows.
pub const MAX_MATCHES: usize = 100;
/// A search stops after looking through this many files.
pub const MAX_FILES: usize = 20_000;
/// A search stops after reading this much in all.
pub const MAX_SEARCH_BYTES: u64 = 64 << 20;
/// A search skips files bigger than this; a read can still open them.
pub const MAX_SEARCH_FILE_BYTES: u64 = 1 << 20;
/// A read looks through no more of a file than this, however far in its
/// lines start or however long they run.
pub const MAX_READ_SCAN_BYTES: u64 = 64 << 20;
/// A search stops after meeting this many folder entries, files or not.
pub const MAX_WALK_ENTRIES: usize = 100_000;
/// A listing looks at no more than this many entries.
pub const MAX_LIST_SCAN: usize = 10_000;
/// The longest text a search looks for.
pub const MAX_PATTERN_CHARS: usize = 200;
/// Build output a search doesn't walk into unless it starts there.
pub const SKIPPED_FOLDERS: &[&str] = &["target", "node_modules"];

/// How much of a file is looked at to tell whether it's text.
const SNIFF_BYTES: usize = 8192;
/// A matching line is shown up to this many characters.
const MATCH_CHARS: usize = 300;

/// The work-area tools, judged by `policy`, which should be the executor's.
/// The root's real path is found on first use; prefer [`tools_at`] when it
/// is already known.
pub fn tools(policy: Policy) -> Vec<Box<dyn Tool>> {
    make(policy, OnceLock::new())
}

/// The work-area tools with the root's real path fixed to `real_root`, as
/// checked at startup, so nothing that changes after can move it.
pub fn tools_at(policy: Policy, real_root: PathBuf) -> Vec<Box<dyn Tool>> {
    make(policy, OnceLock::from(real_root))
}

fn make(policy: Policy, real_root: OnceLock<PathBuf>) -> Vec<Box<dyn Tool>> {
    let area = Arc::new(Area {
        named: policy,
        real_root,
    });
    vec![
        Box::new(ReadFile(area.clone())),
        Box::new(SearchFiles(area)),
    ]
}

/// The executor's walls, twice over. Calls name paths under the root as
/// configured, which is how the executor judges them. What a tool opens is
/// a real path, after links, and is judged against the root's real path,
/// in case the work area itself is reached through a link.
struct Area {
    named: Policy,
    /// The root's real path, found once. Not at startup, since the work
    /// area may not exist yet when the tools are made, and never again, so
    /// a link on the way to it that's pointed elsewhere later can't move
    /// the work area with it.
    real_root: OnceLock<PathBuf>,
}

impl Area {
    /// The walls with the root where it really is.
    fn real(&self) -> Result<Policy, ToolError> {
        let root = match self.real_root.get() {
            Some(root) => root.clone(),
            None => {
                let root = self.named.root.canonicalize().map_err(|e| {
                    ToolError::Failed(format!("couldn't find the work area: {}", e.kind()))
                })?;
                self.real_root.get_or_init(|| root).clone()
            }
        };
        let mut real = self.named.clone();
        real.root = root;
        Ok(real)
    }
}

struct ReadFile(Arc<Area>);
struct SearchFiles(Arc<Area>);

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a file in your work area, with line numbers, or list a folder. \
                          Long files come in parts: give offset (the first line to show) and \
                          limit (how many lines, up to 2000)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative to your work area"},
                    "offset": {"type": "integer", "description": "First line to show, from 1"},
                    "limit": {"type": "integer", "description": "How many lines to show"}
                },
                "required": ["path"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        let path = named_path(&self.0.named, args, true)?;
        line_window(args)?;
        Ok(vec![Effect::ReadPath(path)])
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let path = named_path(&self.0.named, args, true)?;
        let (offset, limit) = line_window(args)?;
        let area = self.0.clone();
        blocking(move || read(&area.real()?, &path, offset, limit)).await
    }
}

#[async_trait]
impl Tool for SearchFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_files".into(),
            description: "Find lines containing some text in the files of your work area. \
                          Ignores case unless you ask. Skips build output (target, \
                          node_modules) unless you search inside it, and large or binary files."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The exact text to look for"},
                    "path": {"type": "string", "description": "A folder or file to search; the whole work area if left out"},
                    "case_sensitive": {"type": "boolean", "description": "Match case exactly"}
                },
                "required": ["text"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        needle(args)?;
        Ok(vec![Effect::ReadPath(named_path(
            &self.0.named,
            args,
            false,
        )?)])
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let start = named_path(&self.0.named, args, false)?;
        let (text, case_sensitive) = needle(args)?;
        let area = self.0.clone();
        blocking(move || search(&area.real()?, &start, &text, case_sensitive)).await
    }
}

/// Runs file work off the async threads, so a big search can't stall the
/// daemon's other work.
async fn blocking<F>(work: F) -> Result<ToolOutput, ToolError>
where
    F: FnOnce() -> Result<ToolOutput, ToolError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| ToolError::Failed("the file work stopped unexpectedly".into()))?
}

/// The path a call names, joined to the work area. Without one, a search
/// covers the whole work area; a read needs one.
fn named_path(policy: &Policy, args: &Value, required: bool) -> Result<PathBuf, ToolError> {
    let path = match args.get("path") {
        None | Some(Value::Null) if !required => return Ok(policy.root.clone()),
        Some(Value::String(path)) => path,
        _ => {
            return Err(ToolError::InvalidArguments(
                "path must be a path in your work area".into(),
            ));
        }
    };
    if path.is_empty() && !required {
        return Ok(policy.root.clone());
    }
    if path.is_empty() || path.contains('\0') {
        return Err(ToolError::InvalidArguments(
            "path must be a path in your work area".into(),
        ));
    }
    Ok(policy.root.join(path))
}

fn line_window(args: &Value) -> Result<(usize, usize), ToolError> {
    let number = |key: &str, default: usize, most: usize| match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=most as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "{key} must be a whole number from 1 to {most}"
                ))
            }),
    };
    Ok((
        number("offset", 1, usize::MAX)?,
        number("limit", DEFAULT_LINES, MAX_LINES)?,
    ))
}

fn needle(args: &Value) -> Result<(String, bool), ToolError> {
    let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
    if text.is_empty() || text.contains('\n') {
        return Err(ToolError::InvalidArguments(
            "text must be one line to look for".into(),
        ));
    }
    if text.chars().count() > MAX_PATTERN_CHARS {
        return Err(ToolError::InvalidArguments(format!(
            "text is at most {MAX_PATTERN_CHARS} characters"
        )));
    }
    let case_sensitive = match args.get("case_sensitive") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ToolError::InvalidArguments(
                "case_sensitive must be true or false".into(),
            ));
        }
    };
    Ok((text.to_string(), case_sensitive))
}

/// Whether the walls let the agent read `path`.
fn readable(policy: &Policy, path: &Path) -> Result<(), String> {
    match policy.check(&Effect::ReadPath(path.to_path_buf())) {
        Decision::Allow => Ok(()),
        Decision::Deny { reason } | Decision::NeedsApproval { reason } => Err(reason),
        Decision::Tripped { breaker } => Err(breaker),
    }
}

/// Where `path` really is, after links, once the walls have judged that too.
fn real_path(policy: &Policy, path: &Path) -> Result<PathBuf, ToolError> {
    let real = path.canonicalize().map_err(|e| {
        ToolError::Failed(match e.kind() {
            std::io::ErrorKind::NotFound => "there's nothing at that path".into(),
            kind => format!("couldn't open it: {kind}"),
        })
    })?;
    readable(policy, &real).map_err(ToolError::Failed)?;
    Ok(real)
}

/// `path` as the agent names it: relative to the work area.
fn shown(policy: &Policy, path: &Path) -> String {
    match path.strip_prefix(&policy.root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// Opens a regular file the walls have already judged by this name. The
/// open doesn't follow a link put there since, doesn't hang on a FIFO, and
/// refuses a file with other names: a hard link could be a file the walls
/// keep the agent from.
fn open_regular(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("it's a link"));
    }
    // Checked before opening too: opening some devices does something.
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(std::io::Error::other("it isn't a regular file"));
    }
    let file = options.open(path)?;
    check_opened(&file, path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("it isn't a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Err(std::io::Error::other(
                "it has other names (hard links), so it isn't opened",
            ));
        }
    }
    Ok(file)
}

/// Opens a folder at `path`, a real path the walls have judged, and a path
/// that lists it. On Linux the listing goes through the open folder itself,
/// so swapping the folder for a link afterwards changes nothing.
fn open_dir(path: &Path) -> std::io::Result<(File, PathBuf)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
    }
    let dir = options.open(path)?;
    check_opened(&dir, path)?;
    #[cfg(target_os = "linux")]
    let listing = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
    };
    #[cfg(not(target_os = "linux"))]
    let listing = path.to_path_buf();
    Ok((dir, listing))
}

/// Refuses an open that ended up somewhere other than `expected`. The
/// walls judged a path; a folder on the way to it could have been swapped
/// for a link before the open, which `O_NOFOLLOW` doesn't catch. The kernel
/// says where the open really landed.
fn check_opened(file: &File, expected: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let at = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        if at != expected {
            return Err(std::io::Error::other("it moved while it was being opened"));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (file, expected);
    Ok(())
}

/// The file's start, if it looks like text, and a reader for all of it.
fn text_reader(file: File) -> std::io::Result<Option<BufReader<File>>> {
    let mut reader = BufReader::with_capacity(SNIFF_BYTES, file);
    let start = reader.fill_buf()?;
    if start.contains(&0) {
        return Ok(None);
    }
    Ok(Some(reader))
}

/// Reads one line, keeping at most `keep` bytes of it and passing over the
/// rest, so one enormous line can't take all the memory. `None` at the end.
fn next_line(reader: &mut impl BufRead, keep: usize) -> std::io::Result<Option<(String, bool)>> {
    let mut bytes = Vec::new();
    let mut cut = false;
    let mut any = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        any = true;
        let newline = buf.iter().position(|&b| b == b'\n');
        let chunk = &buf[..newline.unwrap_or(buf.len())];
        let room = keep.saturating_sub(bytes.len());
        cut |= chunk.len() > room;
        bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
        let used = newline.map_or(buf.len(), |i| i + 1);
        reader.consume(used);
        if newline.is_some() {
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    if !cut && bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    Ok(Some((String::from_utf8_lossy(&bytes).into_owned(), cut)))
}

/// At most `chars` characters of `line`, and whether anything was left out.
fn cut_to(line: &str, chars: usize) -> (&str, bool) {
    match line.char_indices().nth(chars) {
        Some((i, _)) => (&line[..i], true),
        None => (line, false),
    }
}

fn read(
    policy: &Policy,
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<ToolOutput, ToolError> {
    let real = real_path(policy, path)?;
    let name = shown(policy, &real);
    let meta = std::fs::symlink_metadata(&real)
        .map_err(|e| ToolError::Failed(format!("couldn't open {name}: {}", e.kind())))?;
    if meta.is_dir() {
        return list(policy, &real, &name);
    }
    let failed = |e: std::io::Error| ToolError::Failed(format!("couldn't read {name}: {e}"));
    let file = open_regular(&real).map_err(failed)?;
    let size = file.metadata().map_err(failed)?.len();
    let Some(reader) = text_reader(file).map_err(failed)? else {
        return Ok(ToolOutput::text(format!(
            "{name} looks binary ({size} bytes), so it isn't shown."
        )));
    };
    let mut reader = reader.take(MAX_READ_SCAN_BYTES);
    // A line is cut at MAX_LINE_CHARS characters, which is at most four
    // bytes each.
    let keep = MAX_LINE_CHARS * 4;
    let mut out = String::new();
    let mut number = 0;
    let mut shown_lines = 0;
    let mut more = false;
    while let Some((line, cut)) = next_line(&mut reader, keep).map_err(failed)? {
        number += 1;
        if number < offset {
            continue;
        }
        if shown_lines == limit {
            more = true;
            break;
        }
        let (line, short) = cut_to(&line, MAX_LINE_CHARS);
        let entry = format!(
            "{number:>6}\t{line}{}\n",
            if cut || short { " …(line cut)" } else { "" }
        );
        if out.len() + entry.len() > MAX_CHARS && shown_lines > 0 {
            more = true;
            break;
        }
        out.push_str(&entry);
        shown_lines += 1;
    }
    let scanned = MAX_READ_SCAN_BYTES >> 20;
    let stopped = !more && reader.limit() == 0 && size > MAX_READ_SCAN_BYTES;
    if number == 0 {
        return Ok(ToolOutput::text(format!("{name} is empty.")));
    }
    if shown_lines == 0 && stopped {
        return Ok(ToolOutput::text(format!(
            "{name} is too big to read that far: line {offset} isn't in its first {scanned} MiB."
        )));
    }
    if shown_lines == 0 {
        return Ok(ToolOutput::text(format!("{name} has only {number} lines.")));
    }
    if stopped {
        out.push_str(&format!(
            "(Stopped after the first {scanned} MiB of {name}; the rest can't be read here.)\n"
        ));
    }
    if more {
        out.push_str(&format!(
            "(More lines follow. Continue with offset {}.)\n",
            offset + shown_lines
        ));
    }
    Ok(ToolOutput::text(out))
}

fn list(policy: &Policy, dir: &Path, name: &str) -> Result<ToolOutput, ToolError> {
    let failed = |e: std::io::Error| ToolError::Failed(format!("couldn't list {name}: {e}"));
    let (_open, listing) = open_dir(dir).map_err(failed)?;
    let entries = std::fs::read_dir(listing).map_err(failed)?;
    let mut names = Vec::new();
    let mut hidden = 0;
    let mut entries = entries.flatten();
    // Taken a bounded number at a time, so a folder of millions can't
    // take all the memory before the caps below apply.
    for entry in entries.by_ref().take(MAX_LIST_SCAN) {
        let path = dir.join(entry.file_name());
        if readable(policy, &path).is_err() {
            hidden += 1;
            continue;
        }
        let mut shown = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => shown.push('/'),
            Ok(kind) if kind.is_symlink() => shown.push('@'),
            _ => {}
        }
        names.push(shown);
    }
    names.sort();
    let total = names.len();
    let mut out = format!("{name}/\n");
    let mut listed = 0;
    for entry in &names {
        if listed == MAX_ENTRIES || out.len() + entry.len() + 1 > MAX_CHARS {
            break;
        }
        out.push_str(entry);
        out.push('\n');
        listed += 1;
    }
    let unseen = entries.next().is_some();
    if total > listed || unseen {
        let more = total - listed;
        out.push_str(&if unseen {
            format!("(and {more} more, plus others not looked at)\n")
        } else {
            format!("(and {more} more)\n")
        });
    }
    if total == 0 {
        out.push_str("(empty)\n");
    }
    if hidden > 0 {
        out.push_str(&format!(
            "({hidden} not shown: the walls keep you from them)\n"
        ));
    }
    Ok(ToolOutput::text(out))
}

#[derive(Default)]
struct Search {
    matches: Vec<String>,
    chars: usize,
    files: usize,
    bytes: u64,
    large: usize,
    /// Folder entries met so far, files or not.
    entries: usize,
    /// Files and folders that couldn't be opened or read through.
    unreadable: usize,
    stopped: Option<&'static str>,
}

fn search(
    policy: &Policy,
    start: &Path,
    text: &str,
    case_sensitive: bool,
) -> Result<ToolOutput, ToolError> {
    let real = real_path(policy, start)?;
    let wanted = if case_sensitive {
        text.to_string()
    } else {
        text.to_lowercase()
    };
    let mut found = Search::default();
    let meta = std::fs::symlink_metadata(&real)
        .map_err(|e| ToolError::Failed(format!("couldn't open it: {}", e.kind())))?;
    // What was asked for by name has to open, or there's no answer to give.
    let opened = if meta.is_dir() {
        open_dir(&real).map(drop)
    } else {
        open_regular(&real).map(drop)
    };
    opened.map_err(|e| ToolError::Failed(format!("couldn't open it: {e}")))?;
    if meta.is_dir() {
        walk(policy, real, &wanted, case_sensitive, &mut found);
    } else {
        search_file(policy, &real, &wanted, case_sensitive, &mut found);
    }
    let mut out = if found.matches.is_empty() {
        format!("No lines contain {text:?}.\n")
    } else {
        found.matches.join("")
    };
    if let Some(why) = found.stopped {
        out.push_str(&format!(
            "(Stopped early: {why}. Search a smaller folder to see more.)\n"
        ));
    }
    if found.unreadable > 0 {
        out.push_str(&format!(
            "({} {} couldn't be read, so this may be missing matches.)\n",
            found.unreadable,
            if found.unreadable == 1 {
                "file or folder"
            } else {
                "files or folders"
            }
        ));
    }
    if found.large > 0 {
        out.push_str(&format!(
            "({} over {} KiB {} searched.)\n",
            match found.large {
                1 => "1 file".to_string(),
                n => format!("{n} files"),
            },
            MAX_SEARCH_FILE_BYTES >> 10,
            if found.large == 1 {
                "wasn't"
            } else {
                "weren't"
            }
        ));
    }
    Ok(ToolOutput::text(out))
}

/// Searches the files under `start`, a real folder the walls allow, in
/// name order, until a cap stops it.
fn walk(policy: &Policy, start: PathBuf, wanted: &str, case_sensitive: bool, found: &mut Search) {
    let mut folders = vec![start];
    while let Some(dir) = folders.pop() {
        let Ok((_open, listing)) = open_dir(&dir) else {
            found.unreadable += 1;
            continue;
        };
        let Ok(entries) = std::fs::read_dir(listing) else {
            found.unreadable += 1;
            continue;
        };
        // No more than the walk has left, so one enormous folder
        // can't take all the memory before the caps apply.
        let room = MAX_WALK_ENTRIES - found.entries;
        let mut entries: Vec<_> = entries.flatten().take(room + 1).collect();
        let full = entries.len() > room;
        entries.truncate(room);
        found.entries += entries.len();
        entries.sort_by_key(|entry| entry.file_name());
        let mut inner = Vec::new();
        for entry in entries {
            let path = dir.join(entry.file_name());
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // Links aren't followed: what they point at is searched
            // where it really is, if the walls allow it.
            if kind.is_symlink() || readable(policy, &path).is_err() {
                continue;
            }
            if kind.is_dir() {
                let skipped = SKIPPED_FOLDERS
                    .iter()
                    .any(|folder| entry.file_name() == **folder);
                if !skipped {
                    inner.push(path);
                }
            } else if kind.is_file() {
                search_file(policy, &path, wanted, case_sensitive, found);
                if found.stopped.is_some() {
                    break;
                }
            }
        }
        if full && found.stopped.is_none() {
            found.stopped = Some("too many files");
        }
        if found.stopped.is_some() {
            break;
        }
        // Popped from the end, so put them back in reverse to walk in order.
        folders.extend(inner.into_iter().rev());
    }
}

fn search_file(
    policy: &Policy,
    path: &Path,
    wanted: &str,
    case_sensitive: bool,
    found: &mut Search,
) {
    if found.files == MAX_FILES {
        found.stopped = Some("too many files");
        return;
    }
    found.files += 1;
    let Ok(file) = open_regular(path) else {
        found.unreadable += 1;
        return;
    };
    let Ok(size) = file.metadata().map(|m| m.len()) else {
        found.unreadable += 1;
        return;
    };
    if size > MAX_SEARCH_FILE_BYTES {
        found.large += 1;
        return;
    }
    if found.bytes + size > MAX_SEARCH_BYTES {
        found.stopped = Some("too much to read");
        return;
    }
    found.bytes += size;
    let reader = match text_reader(file) {
        Ok(Some(reader)) => reader,
        // Binary, so not searched, by design.
        Ok(None) => return,
        Err(_) => {
            found.unreadable += 1;
            return;
        }
    };
    let name = shown(policy, path);
    // Whole lines are read (the file is small), so a match late in a long
    // line is still found.
    let mut reader = reader.take(MAX_SEARCH_FILE_BYTES);
    let mut number = 0;
    loop {
        let line = match next_line(&mut reader, MAX_SEARCH_FILE_BYTES as usize) {
            Ok(Some((line, _))) => line,
            Ok(None) => break,
            Err(_) => {
                found.unreadable += 1;
                break;
            }
        };
        number += 1;
        let hit = if case_sensitive {
            line.contains(wanted)
        } else {
            line.to_lowercase().contains(wanted)
        };
        if !hit {
            continue;
        }
        if found.matches.len() == MAX_MATCHES {
            found.stopped = Some("too many matches");
            return;
        }
        let (text, cut) = cut_to(line.trim(), MATCH_CHARS);
        let entry = format!("{name}:{number}: {text}{}\n", if cut { " …" } else { "" });
        if found.chars + entry.len() > MAX_CHARS {
            found.stopped = Some("too many matches");
            return;
        }
        found.chars += entry.len();
        found.matches.push(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecOutcome, PolicyExecutor};
    use harness_core::FakeClock;
    use harness_providers::ToolCall;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
        executor: PolicyExecutor,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("work");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let policy = Policy::conservative(&root);
        let mut executor =
            PolicyExecutor::new(policy.clone(), Arc::new(FakeClock::at_default_start()));
        for tool in tools(policy) {
            executor.register(tool);
        }
        Fixture {
            _tmp: tmp,
            root,
            outside,
            executor,
        }
    }

    impl Fixture {
        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }

        async fn call(&mut self, name: &str, arguments: Value) -> ExecOutcome {
            let call = ToolCall {
                id: "1".into(),
                name: name.into(),
                arguments,
            };
            self.executor.execute(&call).await
        }

        async fn text(&mut self, name: &str, arguments: Value) -> String {
            match self.call(name, arguments).await {
                ExecOutcome::Ran(output) => output.content,
                other => panic!("expected the call to run, got {other:?}"),
            }
        }
    }

    fn refused(outcome: &ExecOutcome) -> bool {
        matches!(
            outcome,
            ExecOutcome::Blocked(_) | ExecOutcome::Failed(_) | ExecOutcome::Malformed(_)
        )
    }

    #[tokio::test]
    async fn reads_a_file_in_parts_with_line_numbers() {
        let mut f = fixture();
        f.write("notes.txt", "one\ntwo\r\nthree\nfour\nfive");
        let all = f.text("read_file", json!({"path": "notes.txt"})).await;
        assert_eq!(
            all,
            "     1\tone\n     2\ttwo\n     3\tthree\n     4\tfour\n     5\tfive\n"
        );
        let part = f
            .text(
                "read_file",
                json!({"path": "notes.txt", "offset": 2, "limit": 2}),
            )
            .await;
        assert_eq!(
            part,
            "     2\ttwo\n     3\tthree\n(More lines follow. Continue with offset 4.)\n"
        );
        let past = f
            .text("read_file", json!({"path": "notes.txt", "offset": 9}))
            .await;
        assert_eq!(past, "notes.txt has only 5 lines.");
        f.write("empty.txt", "");
        let empty = f.text("read_file", json!({"path": "empty.txt"})).await;
        assert_eq!(empty, "empty.txt is empty.");
        for bad in [
            json!({"path": "notes.txt", "offset": 0}),
            json!({"path": ""}),
        ] {
            assert!(matches!(
                f.call("read_file", bad).await,
                ExecOutcome::Malformed(_)
            ));
        }
    }

    #[tokio::test]
    async fn long_lines_and_binary_files_stay_small() {
        let mut f = fixture();
        f.write(
            "long.txt",
            &format!("{}\nnext\n", "🦀".repeat(MAX_LINE_CHARS * 3)),
        );
        let text = f.text("read_file", json!({"path": "long.txt"})).await;
        let first = text.lines().next().unwrap();
        assert!(first.ends_with(" …(line cut)"));
        assert_eq!(first.matches('🦀').count(), MAX_LINE_CHARS);
        assert!(text.contains("     2\tnext"));
        std::fs::write(f.root.join("blob.bin"), b"\x7fELF\0\0\x01").unwrap();
        let blob = f.text("read_file", json!({"path": "blob.bin"})).await;
        assert_eq!(blob, "blob.bin looks binary (7 bytes), so it isn't shown.");
        let lines: String = (0..10_000)
            .map(|i| format!("line {i} {}\n", "x".repeat(50)))
            .collect();
        f.write("big.txt", &lines);
        let big = f
            .text("read_file", json!({"path": "big.txt", "limit": MAX_LINES}))
            .await;
        assert!(big.len() <= MAX_CHARS + 100);
        assert!(big.contains("Continue with offset"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_work_area_made_after_the_tools_behind_a_link_works() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, tmp.path().join("link")).unwrap();
        let root = tmp.path().join("link/work");
        let policy = Policy::conservative(&root);
        let mut executor =
            PolicyExecutor::new(policy.clone(), Arc::new(FakeClock::at_default_start()));
        for tool in tools(policy) {
            executor.register(tool);
        }
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("notes.txt"), "hello\n").unwrap();
        for (name, arguments, expected) in [
            ("read_file", json!({"path": "notes.txt"}), "     1\thello\n"),
            (
                "search_files",
                json!({"text": "hello"}),
                "notes.txt:1: hello\n",
            ),
        ] {
            let call = ToolCall {
                id: "1".into(),
                name: name.into(),
                arguments,
            };
            match executor.execute(&call).await {
                ExecOutcome::Ran(output) => assert_eq!(output.content, expected),
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_work_area_stays_where_it_was_first_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (tmp.path().join("first"), tmp.path().join("second"));
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join("notes.txt"), "mine\n").unwrap();
        std::fs::write(second.join("notes.txt"), "someone else's\n").unwrap();
        let link = tmp.path().join("work");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let policy = Policy::conservative(&link);
        let mut executor =
            PolicyExecutor::new(policy.clone(), Arc::new(FakeClock::at_default_start()));
        for tool in tools(policy) {
            executor.register(tool);
        }
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "notes.txt"}),
        };
        assert_eq!(
            executor.execute(&call).await,
            ExecOutcome::Ran(ToolOutput::text("     1\tmine\n"))
        );
        // The link is pointed elsewhere; the work area doesn't follow it.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();
        let outcome = executor.execute(&call).await;
        assert!(
            matches!(&outcome, ExecOutcome::Failed(why) if why.contains("outside")),
            "{outcome:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_open_that_lands_elsewhere_is_refused() {
        // As if a checked folder were swapped for a link before the open:
        // the path judged isn't where the file really is.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("notes.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("swapped")).unwrap();
        let judged = tmp.path().join("swapped/notes.txt");
        let error = open_regular(&judged).unwrap_err();
        assert!(error.to_string().contains("moved"), "{error}");
        assert!(open_regular(&real.join("notes.txt")).is_ok());
        assert!(open_dir(&tmp.path().join("swapped")).is_err());
        assert!(open_dir(&real).is_ok());
    }

    #[tokio::test]
    async fn listings_keep_to_the_character_cap() {
        let mut f = fixture();
        for i in 0..400 {
            f.write(&format!("{i:03}{}", "n".repeat(200)), "");
        }
        let listing = f.text("read_file", json!({"path": "."})).await;
        assert!(listing.len() <= MAX_CHARS + 100, "{}", listing.len());
        let shown = listing.lines().filter(|l| l.ends_with('n')).count();
        assert!(listing.ends_with(&format!("(and {} more)\n", 400 - shown)));
    }

    #[tokio::test]
    async fn lists_a_folder_without_what_the_walls_forbid() {
        let mut f = fixture();
        f.write("src/main.rs", "fn main() {}\n");
        f.write("README.md", "hi\n");
        f.write(".env", "TOKEN=x\n");
        let listing = f.text("read_file", json!({"path": "."})).await;
        assert_eq!(
            listing,
            "./\nREADME.md\nsrc/\n(1 not shown: the walls keep you from them)\n"
        );
    }

    #[tokio::test]
    async fn reads_stop_at_the_walls() {
        let mut f = fixture();
        f.write(".env", "TOKEN=x\n");
        f.write("services/api/.env", "TOKEN=y\n");
        std::fs::write(f.outside.join("secret.txt"), "outside").unwrap();
        for path in [
            ".env",
            "services/api/.env",
            "../outside/secret.txt",
            "/etc/passwd",
        ] {
            let outcome = f.call("read_file", json!({"path": path})).await;
            assert!(
                matches!(outcome, ExecOutcome::Blocked(_)),
                "{path}: {outcome:?}"
            );
        }
        #[cfg(unix)]
        {
            // Links to forbidden files are refused, whatever they're called.
            std::os::unix::fs::symlink(f.outside.join("secret.txt"), f.root.join("out.txt"))
                .unwrap();
            std::os::unix::fs::symlink(f.root.join(".env"), f.root.join("env.txt")).unwrap();
            std::os::unix::fs::symlink(&f.outside, f.root.join("elsewhere")).unwrap();
            for path in ["out.txt", "env.txt", "elsewhere", "elsewhere/secret.txt"] {
                let outcome = f.call("read_file", json!({"path": path})).await;
                assert!(refused(&outcome), "{path}: {outcome:?}");
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn other_names_and_special_files_are_refused() {
        let mut f = fixture();
        let secret = f.write(".env", "TOKEN=x\n");
        std::fs::hard_link(&secret, f.root.join("notes.txt")).unwrap();
        let outcome = f.call("read_file", json!({"path": "notes.txt"})).await;
        assert!(
            matches!(&outcome, ExecOutcome::Failed(why) if why.contains("other names")),
            "{outcome:?}"
        );
        // A FIFO would hang a plain open; this one is refused at once.
        let fifo = f.root.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let outcome = f.call("read_file", json!({"path": "pipe"})).await;
        assert!(
            matches!(&outcome, ExecOutcome::Failed(why) if why.contains("regular file")),
            "{outcome:?}"
        );
        // A search says what it couldn't read (the linked file; the pipe
        // isn't a file to search) rather than passing it over.
        let found = f.text("search_files", json!({"text": "token"})).await;
        assert_eq!(
            found,
            "No lines contain \"token\".\n\
             (1 file or folder couldn't be read, so this may be missing matches.)\n"
        );
        let outcome = f
            .call("search_files", json!({"text": "token", "path": "pipe"}))
            .await;
        assert!(
            matches!(&outcome, ExecOutcome::Failed(why) if why.contains("couldn't open it")),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn search_finds_lines_and_keeps_to_the_walls() {
        let mut f = fixture();
        f.write("src/lib.rs", "// The Token is checked here.\nfn ok() {}\n");
        f.write("docs/notes.md", "nothing\nset TOKEN before running\n");
        f.write(".env", "TOKEN=x\n");
        f.write(".git/config", "token = y\n");
        f.write("target/debug/out.txt", "token in build output\n");
        std::fs::write(f.outside.join("secret.txt"), "token outside\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&f.outside, f.root.join("elsewhere")).unwrap();
        let found = f.text("search_files", json!({"text": "token"})).await;
        assert_eq!(
            found,
            "docs/notes.md:2: set TOKEN before running\n\
             src/lib.rs:1: // The Token is checked here.\n"
        );
        let exact = f
            .text(
                "search_files",
                json!({"text": "Token", "case_sensitive": true}),
            )
            .await;
        assert_eq!(exact, "src/lib.rs:1: // The Token is checked here.\n");
        // Build output is searched when asked for by name.
        let build = f
            .text("search_files", json!({"text": "token", "path": "target"}))
            .await;
        assert_eq!(build, "target/debug/out.txt:1: token in build output\n");
        for path in [".git", "../outside", ".env"] {
            let outcome = f
                .call("search_files", json!({"text": "token", "path": path}))
                .await;
            assert!(refused(&outcome), "{path}: {outcome:?}");
        }
        assert!(matches!(
            f.call("search_files", json!({"text": ""})).await,
            ExecOutcome::Malformed(_)
        ));
    }

    #[test]
    fn folders_are_read_only_as_far_as_the_caps() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for i in 0..MAX_LIST_SCAN + 5 {
            std::fs::write(root.join(format!("{i:05}")), "").unwrap();
        }
        let policy = Policy::conservative(&root);
        let listing = list(&policy, &root, ".").unwrap().content;
        assert!(listing.contains("plus others not looked at"), "{listing}");
        let mut found = Search {
            entries: MAX_WALK_ENTRIES - 10,
            ..Search::default()
        };
        walk(&policy, root.clone(), "x", false, &mut found);
        assert_eq!(found.entries, MAX_WALK_ENTRIES);
        assert_eq!(found.files, 10);
        assert_eq!(found.stopped, Some("too many files"));
    }

    #[tokio::test]
    async fn reads_look_through_only_so_much_of_a_file() {
        let mut f = fixture();
        // One line longer than the whole budget, then a short one.
        let path = f.root.join("minified.js");
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&path).unwrap();
            let chunk = vec![b'x'; 1 << 20];
            for _ in 0..(MAX_READ_SCAN_BYTES >> 20) + 1 {
                file.write_all(&chunk).unwrap();
            }
            file.write_all(b"\nlast\n").unwrap();
        }
        let first = f.text("read_file", json!({"path": "minified.js"})).await;
        assert!(
            first.contains(" …(line cut)"),
            "{}",
            &first[first.len() - 200..]
        );
        assert!(
            first.ends_with("the rest can't be read here.)\n"),
            "{first:.100}"
        );
        let far = f
            .text("read_file", json!({"path": "minified.js", "offset": 2}))
            .await;
        assert_eq!(
            far,
            "minified.js is too big to read that far: line 2 isn't in its first 64 MiB."
        );
    }

    #[tokio::test]
    async fn search_stops_at_its_caps() {
        let mut f = fixture();
        let many: String = (0..MAX_MATCHES + 50)
            .map(|i| format!("hit {i}\n"))
            .collect();
        f.write("many.txt", &many);
        f.write("huge.txt", &"hit\n".repeat(MAX_SEARCH_FILE_BYTES as usize));
        let found = f.text("search_files", json!({"text": "hit"})).await;
        assert_eq!(found.matches("many.txt:").count(), MAX_MATCHES);
        assert!(found.contains("Stopped early: too many matches"));
        let huge = f
            .text("search_files", json!({"text": "hit", "path": "huge.txt"}))
            .await;
        assert!(huge.contains("1 file over 1024 KiB wasn't searched"));
    }
}
