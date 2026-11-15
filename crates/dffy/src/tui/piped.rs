//! A diff read from a pipe, fed to the viewer as it arrives: commits, the
//! text between files, and files, in input order. A thread parses the
//! input and sends each piece as an [`Event::Piped`]; it waits while it's
//! [`READ_AHEAD`] entries past where the viewer has been, so the writer
//! (`git log -p`, endless) blocks on a full pipe instead of dffy holding
//! the whole log.
//!
//! A file's patch goes to an unlinked spill file as soon as it's parsed,
//! and comes back when the file is drawn: memory holds each file's header
//! and counts, not its text. A piped file is partial. It has only its
//! hunks, so its unchanged lines can't be revealed, it can't be syntax
//! diffed, and its hunk headings are the ones git wrote.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;

use dffy_core::bstr::{BString, ByteSlice};
use dffy_core::diff::{Hunk, Line, LineKind, Text};
use dffy_core::git::{FileChange, FileDiff, Mode, Side, Status};

use super::Event;
use super::view::Counts;
use crate::pager::for_each_line;
use crate::pager::parse::{self, FileInfo, Parser, TextKind};
use crate::pager::stdin::Stdin;
use crate::render::{FileStatus, Kind};

/// Entries read past the one the viewer shows.
pub(crate) const READ_AHEAD: usize = 128;

/// While reading to the end, entries read past those the viewer has taken
/// in. Keys share the viewer's queue with the entries, so a key waits
/// behind at most this many.
pub(crate) const READ_ALL_AHEAD: usize = 4096;

/// Entries read at most, whatever the viewer asks for: the picker and the
/// tree filter read on to the end of the input while they're open, and an
/// endless writer would otherwise grow the list without bound. Their
/// patches are on disk; what stays is a few hundred bytes of each one's
/// header, counts and tree node.
pub(crate) const MAX_ENTRIES: usize = 1_000_000;

/// The line a piped file's hunks may start at, at most. A file is read back
/// over its real line numbers, at about 9 bytes per line before a hunk, so
/// this caps one file at about 150MB.
const MAX_LINES: usize = 1 << 24;

/// Lines of text with what each one is.
type Lines = Vec<(TextKind, BString)>;

pub(crate) enum Item {
    Commit(Arc<Commit>),
    /// A file as the rest of the viewer knows files, and what's piped
    /// about it. The paths live in the change alone: a long input holds
    /// many of these.
    File(FileChange, Arc<PipedFile>),
}

/// A commit's header and message, or text that came between files.
#[derive(Debug)]
pub(crate) struct Commit {
    /// The commit's full id; `None` for text that isn't a commit.
    pub id: Option<String>,
    pub subject: String,
    pub lines: Lines,
}

impl Commit {
    /// The commit as git prints it: id, metadata and message, as the
    /// lines `git show` starts with.
    pub(crate) fn new(id: String, lines: Lines) -> Self {
        let subject = lines
            .iter()
            .find(|(kind, text)| {
                matches!(kind, TextKind::Plain | TextKind::CommitMessage) && !text.trim().is_empty()
            })
            .map(|(_, text)| text.trim().to_str_lossy().into_owned())
            .unwrap_or_default();
        Self {
            id: Some(id),
            subject,
            lines,
        }
    }

    /// The tree's and the title's name for it: the short id and the
    /// subject, or the text's first line.
    pub(crate) fn label(&self) -> BString {
        match &self.id {
            Some(id) => format!("{} {}", &id[..id.len().min(7)], self.subject).into(),
            None => self
                .lines
                .first()
                .map(|(_, text)| text.trim().into())
                .unwrap_or_default(),
        }
    }
}

/// What's known of a piped file besides its [`FileChange`].
pub(crate) struct PipedFile {
    /// As the patch's header said; finer than the change's status.
    pub status: FileStatus,
    pub binary: bool,
    pub counts: Counts,
    body: Body,
    /// A combined diff's lines (`diff --cc`), shown as they came, each
    /// with its number of prefix columns (see [`parse::Event::Combined`]).
    pub combined: Option<Vec<(usize, BString)>>,
}

/// A file's hunks as the patch had them, colors removed.
enum Body {
    Spilled {
        spill: Arc<File>,
        offset: u64,
        len: usize,
    },
    Kept(Box<[u8]>),
}

/// The file a patch header describes, as the rest of the viewer knows
/// files. The sides have no object ids; nothing reads them for a piped
/// file.
fn change(info: &FileInfo) -> FileChange {
    let mode = |mode: Option<&str>| match mode {
        Some("100755") => Mode::Executable,
        Some("120000") => Mode::Symlink,
        Some("160000") => Mode::Gitlink,
        _ => Mode::File,
    };
    let side = |m: Option<&String>| {
        Some(Side::Worktree {
            mode: mode(m.map(String::as_str)),
            id: None,
        })
    };
    let (old, new) = (side(info.old_mode.as_ref()), side(info.new_mode.as_ref()));
    let path = |p: &[u8]| BString::from(p);
    let (path, status, old, new) = match info.status {
        FileStatus::Added | FileStatus::Untracked => {
            (path(&info.new_path), Status::Added, None, new)
        }
        FileStatus::Deleted => (path(&info.old_path), Status::Deleted, old, None),
        FileStatus::Renamed | FileStatus::Copied => (
            path(&info.new_path),
            Status::Renamed {
                from: path(&info.old_path),
                similarity: info.similarity.unwrap_or(0),
            },
            old,
            new,
        ),
        FileStatus::TypeChange => (path(&info.new_path), Status::TypeChange, old, new),
        FileStatus::Modified | FileStatus::Combined => {
            (path(&info.new_path), Status::Modified, old, new)
        }
    };
    FileChange {
        path,
        status,
        old,
        new,
        staging: None,
    }
}

impl PipedFile {
    /// Reads the file's hunks back and lays them out over its real line
    /// numbers: each side's text holds the lines the hunks show, at their
    /// place, and an empty line for each line the patch left out. Those are
    /// never shown, since a piped file's gaps don't open. Also returns
    /// each hunk's heading as git wrote it.
    pub(crate) fn diff(&self, change: FileChange) -> io::Result<(FileDiff, Vec<BString>)> {
        let body = match &self.body {
            Body::Spilled { spill, offset, len } => {
                let mut bytes = vec![0; *len];
                spill.read_exact_at(&mut bytes, *offset)?;
                bytes
            }
            Body::Kept(bytes) => bytes.to_vec(),
        };
        let (mut old, mut new) = (Vec::new(), Vec::new());
        let (mut old_n, mut new_n) = (0, 0);
        let mut hunks = Vec::new();
        let mut headings = Vec::new();
        // The side or sides of the last line, for a no-newline marker.
        let mut last = None;
        // Split on newlines alone: a carriage return is content.
        for line in body
            .strip_suffix(b"\n")
            .unwrap_or(&body)
            .split(|&b| b == b'\n')
        {
            if let Some(h) = parse::parse_hunk_header(line) {
                let zero = |start: usize, len: usize| {
                    if len == 0 {
                        start
                    } else {
                        start.saturating_sub(1)
                    }
                };
                let (os, ns) = (zero(h.old_start, h.old_len), zero(h.new_start, h.new_len));
                if os.max(ns) > MAX_LINES {
                    return Err(io::Error::other(format!(
                        "a hunk starts past line {MAX_LINES}, more than a piped file can hold"
                    )));
                }
                // A byte and a line start per line left out: a hunk at line
                // 1,000,000 costs 9MB while the file is loaded. A sparse
                // text is the upgrade if that shows up.
                old.resize(old.len() + os.saturating_sub(old_n), b'\n');
                new.resize(new.len() + ns.saturating_sub(new_n), b'\n');
                (old_n, new_n) = (old_n.max(os), new_n.max(ns));
                hunks.push(Hunk {
                    old_start: old_n,
                    old_len: h.old_len,
                    new_start: new_n,
                    new_len: h.new_len,
                    lines: Vec::new(),
                });
                headings.push(BString::from(h.heading));
                last = None;
                continue;
            }
            let Some(hunk) = hunks.last_mut() else {
                continue;
            };
            let (kind, text) = match line.split_first() {
                Some((b'-', text)) => (LineKind::Removed, text),
                Some((b'+', text)) => (LineKind::Added, text),
                Some((b'\\', _)) => {
                    // The line before has no newline on its side(s).
                    if matches!(last, Some(LineKind::Removed | LineKind::Context)) {
                        old.pop();
                    }
                    if matches!(last, Some(LineKind::Added | LineKind::Context)) {
                        new.pop();
                    }
                    last = None;
                    continue;
                }
                Some((_, text)) => (LineKind::Context, text),
                None => (LineKind::Context, line),
            };
            hunk.lines.push(Line {
                kind,
                old: old_n,
                new: new_n,
                emphasis: Vec::new(),
                unchanged: dffy_core::diff::Unchanged::No,
                block_start: false,
                paired: false,
            });
            if kind != LineKind::Added {
                old.extend_from_slice(text);
                old.push(b'\n');
                old_n += 1;
            }
            if kind != LineKind::Removed {
                new.extend_from_slice(text);
                new.push(b'\n');
                new_n += 1;
            }
            last = Some(kind);
        }
        Ok((
            FileDiff {
                change,
                old: Text::new(old),
                new: Text::new(new),
                binary: self.binary,
                too_large: false,
                note: None,
                hunks,
            },
            headings,
        ))
    }
}

/// How far the parser may read: it waits while it has sent `limit`
/// entries (or [`MAX_ENTRIES`]), until the viewer raises it.
#[derive(Default)]
pub(crate) struct Gate {
    limit: Mutex<usize>,
    raised: Condvar,
}

impl Gate {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit: Mutex::new(limit),
            raised: Condvar::new(),
        }
    }

    /// Lets the parser read until `limit` entries are sent. A lower limit
    /// than before stops it once it gets there; what it sent stays sent.
    pub(crate) fn want(&self, limit: usize) {
        let mut current = self.limit.lock().unwrap_or_else(PoisonError::into_inner);
        if limit > *current {
            self.raised.notify_all();
        }
        *current = limit;
    }

    fn wait(&self, sent: usize) {
        let mut limit = self.limit.lock().unwrap_or_else(PoisonError::into_inner);
        while sent >= (*limit).min(MAX_ENTRIES) {
            limit = self
                .raised
                .wait(limit)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Why the parser stopped early.
enum Stop {
    /// The viewer is gone.
    Gone,
    Io(io::Error),
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Parses `stdin` on a thread, sending [`Event::Piped`] for each entry and
/// [`Event::EnumerationDone`] at the end, with the error if reading failed.
pub(crate) fn feed(stdin: Stdin, tx: Sender<Event>, gate: Arc<Gate>) -> io::Result<()> {
    thread::Builder::new()
        .name("dffy-piped".into())
        .spawn(move || {
            let mut builder = Builder {
                tx,
                gate,
                spill: Spill::new(),
                sent: 0,
                pending: None,
                file: None,
            };
            let mut parser = Parser::default();
            let result = for_each_line(
                stdin,
                || Ok::<(), Stop>(()),
                |line| parser.feed(line, &mut |event| builder.event(event)),
            )
            .and_then(|()| parser.finish(&mut |event| builder.event(event)))
            .and_then(|()| builder.finish());
            let error = match result {
                Ok(()) => None,
                Err(Stop::Gone) => return,
                Err(Stop::Io(e)) => Some(format!("reading the diff failed: {e}")),
            };
            builder.tx.send(Event::EnumerationDone(error)).ok();
        })?;
    Ok(())
}

struct Building {
    info: FileInfo,
    body: Vec<u8>,
    counts: Counts,
    combined: Option<Vec<(usize, BString)>>,
}

struct Builder {
    tx: Sender<Event>,
    gate: Arc<Gate>,
    spill: Spill,
    sent: usize,
    /// A commit's id (`None` for a text block) and its lines, sent once
    /// something else starts.
    pending: Option<(Option<String>, Lines)>,
    file: Option<Building>,
}

impl Builder {
    fn event(&mut self, event: parse::Event) -> Result<(), Stop> {
        match event {
            parse::Event::Text(kind, line) => {
                self.send_file()?;
                if kind == TextKind::CommitHeader {
                    self.send_pending()?;
                    let id = line[b"commit ".len()..]
                        .iter()
                        .take_while(|b| b.is_ascii_hexdigit())
                        .map(|&b| char::from(b))
                        .collect();
                    self.pending = Some((Some(id), Vec::new()));
                }
                // Blank lines don't start a block of text of their own.
                if self.pending.is_none() && line.trim().is_empty() {
                    return Ok(());
                }
                let (_, lines) = self.pending.get_or_insert_with(|| (None, Vec::new()));
                lines.push((kind, line.into()));
            }
            parse::Event::File(info) => {
                self.send_file()?;
                self.send_pending()?;
                self.file = Some(Building {
                    info: info.clone(),
                    body: Vec::new(),
                    counts: Counts::default(),
                    combined: (info.status == FileStatus::Combined).then(Vec::new),
                });
            }
            parse::Event::Hunk(h) => {
                if let Some(f) = &mut self.file {
                    let numbers = crate::render::HunkHeader {
                        old_start: h.old_start,
                        old_len: h.old_len,
                        new_start: h.new_start,
                        new_len: h.new_len,
                        heading: h.heading,
                        hidden: 0,
                    }
                    .numbers();
                    f.body.extend_from_slice(numbers.as_bytes());
                    if !h.heading.is_empty() {
                        f.body.push(b' ');
                        f.body.extend_from_slice(h.heading);
                    }
                    f.body.push(b'\n');
                }
            }
            parse::Event::Line(kind, text) => {
                if let Some(f) = &mut self.file {
                    let prefix = match kind {
                        Kind::Context => Some(b' '),
                        Kind::Removed => {
                            f.counts.removed += 1;
                            Some(b'-')
                        }
                        Kind::Added => {
                            f.counts.added += 1;
                            Some(b'+')
                        }
                        // The whole marker line.
                        Kind::NoNewline => None,
                    };
                    f.body.extend(prefix);
                    f.body.extend_from_slice(text);
                    f.body.push(b'\n');
                }
            }
            // The body is read back as hunks, which a stray line isn't part
            // of; it changes nothing.
            parse::Event::HunkEnd | parse::Event::Stray(_) => {}
            parse::Event::Combined { parents, line } => {
                if let Some(f) = &mut self.file {
                    let prefix = &line[..parents.min(line.len())];
                    if !line.starts_with(b"@@@") && parents > 0 {
                        f.counts.added += usize::from(prefix.contains(&b'+'));
                        f.counts.removed +=
                            usize::from(prefix.contains(&b'-') && !prefix.contains(&b'+'));
                    }
                    f.combined
                        .get_or_insert_with(Vec::new)
                        .push((parents, line.into()));
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), Stop> {
        self.send_file()?;
        self.send_pending()
    }

    fn send(&mut self, item: Item) -> Result<(), Stop> {
        self.gate.wait(self.sent);
        self.tx.send(Event::Piped(item)).map_err(|_| Stop::Gone)?;
        self.sent += 1;
        Ok(())
    }

    fn send_pending(&mut self) -> Result<(), Stop> {
        let Some((id, mut lines)) = self.pending.take() else {
            return Ok(());
        };
        while lines.last().is_some_and(|(_, l)| l.trim().is_empty()) {
            lines.pop();
        }
        let commit = match id {
            Some(id) => Commit::new(id, lines),
            None if lines.is_empty() => return Ok(()),
            None => Commit {
                id: None,
                subject: String::new(),
                lines,
            },
        };
        self.send(Item::Commit(Arc::new(commit)))
    }

    fn send_file(&mut self) -> Result<(), Stop> {
        let Some(f) = self.file.take() else {
            return Ok(());
        };
        let body = self.spill.store(f.body);
        self.send(Item::File(
            change(&f.info),
            Arc::new(PipedFile {
                status: f.info.status,
                binary: f.info.binary,
                counts: f.counts,
                body,
                combined: f.combined,
            }),
        ))
    }
}

/// Where patches wait to be drawn: a file removed from its directory as
/// soon as it's open, so it goes away with dffy however dffy exits.
struct Spill {
    /// `None` when no temporary file could be made; patches stay in memory.
    file: Option<Arc<File>>,
    len: u64,
}

impl Spill {
    /// In dffy's cache directory: the temporary directory is often a tmpfs,
    /// which would put the patches back in memory.
    fn new() -> Self {
        let dir = dffy_core::cache::Cache::default_dir()
            .filter(|dir| std::fs::create_dir_all(dir).is_ok())
            .unwrap_or_else(std::env::temp_dir);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = dir.join(format!("dffy-{}-{nanos}.patch", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()
            .filter(|_| std::fs::remove_file(&path).is_ok());
        Self {
            file: file.map(Arc::new),
            len: 0,
        }
    }

    fn store(&mut self, bytes: Vec<u8>) -> Body {
        if let Some(file) = &self.file
            && file.write_all_at(&bytes, self.len).is_ok()
        {
            let offset = self.len;
            self.len += bytes.len() as u64;
            return Body::Spilled {
                spill: Arc::clone(file),
                offset,
                len: bytes.len(),
            };
        }
        Body::Kept(bytes.into_boxed_slice())
    }
}

/// The entries of `input`, parsed on this thread, patches kept in memory.
#[cfg(test)]
pub(crate) fn items(input: &[u8]) -> Vec<Item> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut builder = Builder {
        tx,
        gate: Arc::new(Gate::new(usize::MAX)),
        spill: Spill { file: None, len: 0 },
        sent: 0,
        pending: None,
        file: None,
    };
    let mut parser = Parser::default();
    let parsed = for_each_line(
        input,
        || Ok::<(), Stop>(()),
        |line| parser.feed(line, &mut |event| builder.event(event)),
    )
    .and_then(|()| parser.finish(&mut |event| builder.event(event)))
    .and_then(|()| builder.finish());
    assert!(parsed.is_ok(), "in-memory input");
    drop(builder);
    rx.into_iter()
        .filter_map(|event| match event {
            Event::Piped(item) => Some(item),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log's commits come first and their files after them, in order;
    /// a file without hunks (a mode change) is an entry too.
    #[test]
    fn a_log_parses_into_commits_and_their_files() {
        let log = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/git-log-color.diff"
        ));
        let names: Vec<String> = items(log)
            .iter()
            .map(|item| match item {
                Item::Commit(c) => format!("commit {}", c.label()),
                Item::File(c, f) => {
                    format!("  {} +{} -{}", c.path, f.counts.added, f.counts.removed)
                }
            })
            .collect();
        insta::assert_snapshot!(names.join("\n"), @r"
        commit 1fcf726 Add a third line to the notes
          notes.txt +1 -0
        commit 80f116f Widen calc to long and tidy files
          added.md +1 -0
          build.sh +0 -0
          calc.c +4 -3
          gone.txt +0 -1
          greetings.py +1 -1
          logo.png +0 -0
          notes.txt +1 -1
        ");
    }

    fn file(body: &[u8]) -> (FileChange, PipedFile) {
        let info = FileInfo::new(b"a.c".to_vec(), b"a.c".to_vec());
        let piped = PipedFile {
            status: FileStatus::Modified,
            binary: false,
            counts: Counts::default(),
            body: Body::Kept(body.into()),
            combined: None,
        };
        (change(&info), piped)
    }

    /// Lines keep their real numbers, the space between hunks stays empty,
    /// and a no-newline marker takes the newline off its line.
    #[test]
    fn a_stored_patch_reads_back_at_its_line_numbers() {
        let (change, piped) = file(
            b"@@ -3,2 +3,2 @@ int main()\n a\n-b\n+B\n@@ -10 +10,2 @@\n x\n+y\n\\ No newline at end of file\n",
        );
        let (diff, headings) = piped.diff(change).expect("in memory");
        assert_eq!(headings, [BString::from("int main()"), BString::from("")]);
        let (h0, h1) = (&diff.hunks[0], &diff.hunks[1]);
        assert_eq!(
            (h0.old_start, h0.new_start, h1.old_start, h1.new_start),
            (2, 2, 9, 9)
        );
        assert_eq!(diff.old.line(3), b"b\n");
        assert_eq!(diff.new.line(3), b"B\n");
        assert_eq!(diff.old.line(5), b"\n", "a line the patch left out");
        assert_eq!(diff.old.len(), 10);
        assert_eq!(diff.new.line(10), b"y", "no newline");
        let added = &h1.lines[1];
        assert_eq!((added.kind, added.new), (LineKind::Added, 10));
    }

    /// A hunk far down a file, as a corrupt or hostile patch can claim, is
    /// refused instead of filling memory with the lines before it.
    #[test]
    fn a_hunk_past_the_line_cap_is_refused() {
        let (change, piped) = file(b"@@ -10000000000,2 +1,2 @@\n a\n b\n");
        assert!(piped.diff(change).is_err());
    }

    /// A binary file is an entry with its note; a merge's combined diff is
    /// kept as its lines, never read as two sides.
    #[test]
    fn binary_and_combined_diffs_are_kept_as_they_came() {
        let binary = b"diff --git a/logo.png b/logo.png\nindex 1..2 100644\n\
            Binary files a/logo.png and b/logo.png differ\n";
        let [Item::File(change, file)] = &items(binary)[..] else {
            panic!("one file");
        };
        assert!(file.binary);
        let loaded = super::super::view::Loaded::piped(
            file.diff(change.clone()).expect("in memory").0,
            Vec::new(),
        );
        assert!(loaded.diff.binary && loaded.diff.hunks.is_empty());

        let merge = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/git-show-merge.diff"
        ));
        let combined: Vec<String> = items(merge)
            .iter()
            .filter_map(|item| match item {
                Item::File(_, f) => f.combined.as_ref(),
                Item::Commit(_) => None,
            })
            .flatten()
            .map(|(parents, line)| {
                let role = crate::pager::combined_role(*parents, line);
                format!("{} {line}", role.key())
            })
            .collect();
        insta::assert_snapshot!(combined.join("\n"), @r"
        dim index e0fe810,218dcf7..2f61be3
        dim --- a/notes.txt
        dim +++ b/notes.txt
        hunk-header @@@ -1,3 -1,3 +1,4 @@@
        context   first line
        context   second line, edited
        added  +third line
        added + side line
        ");
    }

    /// The parser waits at the cap whatever the viewer asks, and a lowered
    /// limit holds it until the viewer raises it again.
    #[test]
    fn the_gate_holds_at_the_cap_and_at_a_lowered_limit() {
        use std::time::Duration;
        let waits = |gate: &Arc<Gate>, sent: usize| {
            let (tx, rx) = std::sync::mpsc::channel();
            let gate = Arc::clone(gate);
            thread::spawn(move || {
                gate.wait(sent);
                let _ = tx.send(());
            });
            rx
        };
        let gate = Arc::new(Gate::new(usize::MAX));
        let capped = waits(&gate, MAX_ENTRIES);
        assert!(capped.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(
            waits(&gate, 500)
                .recv_timeout(Duration::from_secs(5))
                .is_ok()
        );
        gate.want(10);
        let lowered = waits(&gate, 10);
        assert!(lowered.recv_timeout(Duration::from_millis(100)).is_err());
        gate.want(11);
        assert!(lowered.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn new_files_start_at_line_one() {
        let (change, piped) = file(b"@@ -0,0 +1,2 @@\n+a\n+b\n");
        let (diff, _) = piped.diff(change).expect("in memory");
        assert_eq!((diff.hunks[0].old_start, diff.hunks[0].new_start), (0, 0));
        assert_eq!(diff.new.len(), 2);
        assert!(diff.old.is_empty());
    }
}
