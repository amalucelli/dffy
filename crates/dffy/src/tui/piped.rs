//! A diff read from a pipe, fed to the viewer as it arrives: commits, the
//! text between files, and files, in input order. A thread parses the
//! input into [`patch::Item`]s and sends each one as an [`Event::Piped`];
//! it waits while it's [`READ_AHEAD`] entries past where the viewer has
//! been, so the writer (`git log -p`, endless) blocks on a full pipe
//! instead of dffy holding the whole log.
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

use dffy_core::document::{self, Counts};
use dffy_core::git::FileChange;
use dffy_core::patch::{self, Builder, Commit, Parser};

use super::Event;
use crate::pager::for_each_line;
use crate::pager::stdin::Stdin;

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

pub(crate) enum Item {
    Commit(Arc<Commit>),
    /// A file as the rest of the viewer knows files, and what's piped
    /// about it. The paths live in the change alone: a long input holds
    /// many of these.
    File(FileChange, Arc<PipedFile>),
}

/// What's known of a piped file besides its [`FileChange`]; its hunks are
/// in the spill.
pub(crate) type PipedFile = document::Piped<Body>;

/// A file's hunks as the patch had them, colors removed.
#[derive(Debug)]
pub(crate) enum Body {
    Spilled {
        spill: Arc<File>,
        offset: u64,
        len: usize,
    },
    Kept(Box<[u8]>),
}

/// Reads `piped`'s hunks back as `change`'s diff (see
/// [`document::File::piped`]).
pub(crate) fn read(piped: &PipedFile, change: FileChange) -> io::Result<document::File> {
    let read;
    let body: &[u8] = match &piped.patch {
        Body::Spilled { spill, offset, len } => {
            let mut bytes = vec![0; *len];
            spill.read_exact_at(&mut bytes, *offset)?;
            read = bytes;
            &read
        }
        Body::Kept(bytes) => bytes,
    };
    let sides = patch::sides(body).map_err(io::Error::other)?;
    Ok(document::File::piped(change, sides, piped.binary))
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
            let mut sink = Sink {
                tx,
                gate,
                spill: Spill::new(),
                sent: 0,
            };
            let error = match parse(stdin, &mut sink) {
                Ok(()) => None,
                Err(Stop::Gone) => return,
                Err(Stop::Io(e)) => Some(format!("reading the diff failed: {e}")),
            };
            sink.tx.send(Event::EnumerationDone(error)).ok();
        })?;
    Ok(())
}

/// Parses `input` into entries for `sink`.
fn parse(input: impl io::Read, sink: &mut Sink) -> Result<(), Stop> {
    let mut parser = Parser::default();
    let mut builder = Builder::default();
    let mut send = |item| sink.send(item);
    for_each_line(
        input,
        || Ok::<(), Stop>(()),
        |line| parser.feed(line, &mut |event| builder.event(event, &mut send)),
    )?;
    parser.finish(&mut |event| builder.event(event, &mut send))?;
    builder.finish(&mut send)
}

/// Where entries go: their patches to the spill, the rest to the viewer,
/// as far as the gate lets them.
struct Sink {
    tx: Sender<Event>,
    gate: Arc<Gate>,
    spill: Spill,
    sent: usize,
}

impl Sink {
    fn send(&mut self, item: patch::Item) -> Result<(), Stop> {
        let item = match item {
            patch::Item::Commit(commit) => Item::Commit(Arc::new(commit)),
            patch::Item::File(f) => {
                let file = PipedFile {
                    status: f.info.status,
                    binary: f.info.binary,
                    counts: Counts {
                        added: f.added,
                        removed: f.removed,
                    },
                    patch: self.spill.store(f.hunks),
                    combined: f.combined,
                };
                Item::File(document::file_change(f.info), Arc::new(file))
            }
        };
        self.gate.wait(self.sent);
        self.tx.send(Event::Piped(item)).map_err(|_| Stop::Gone)?;
        self.sent += 1;
        Ok(())
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
    let mut sink = Sink {
        tx,
        gate: Arc::new(Gate::new(usize::MAX)),
        spill: Spill { file: None, len: 0 },
        sent: 0,
    };
    assert!(parse(input, &mut sink).is_ok(), "in-memory input");
    drop(sink);
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
                Item::Commit(c) => format!("commit {}", String::from_utf8_lossy(&c.label())),
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
        let loaded = read(file, change.clone()).expect("in memory");
        assert!(loaded.diff().binary && loaded.diff().hunks.is_empty());

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
                let role = crate::pager::combined_role(patch::combined_kind(*parents, line));
                format!("{} {}", role.key(), String::from_utf8_lossy(line))
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
}
