//! Standard input when it may hold a diff: read on a thread of its own so
//! the decisions made before any output (is this a diff, does it fit one
//! screen) can give up waiting on a slow writer, and with a short queue so
//! a reader that stops consuming stops the writer too.

use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use dffy_core::patch::{Event, Parser, strip_ansi};
use dffy_core::view::Kind;

/// Bytes per read from stdin.
const CHUNK: usize = 64 << 10;
/// Chunks read ahead of whoever consumes them; past this the reader waits,
/// the pipe fills, and the writer blocks.
const QUEUE: usize = 4;

/// How much is looked at to tell a diff from other text: git's own output
/// puts the first diff marker within a few lines.
const SNIFF_BYTES: usize = 64 << 10;
const SNIFF_LINES: usize = 200;
/// `git log -p` may open with merges, which show no diff, and long
/// messages; input that starts like a log is given this much instead.
const SNIFF_LOG_BYTES: usize = 1 << 20;
/// Once input has started, how long to wait for enough of it to decide.
/// A slow writer (a pickaxe search) gets passed through after this.
const SNIFF_WAIT: Duration = Duration::from_millis(500);
/// How long a diff may keep trickling in before the viewer opens without
/// knowing whether it would have fit one screen.
const FIT_WAIT: Duration = Duration::from_millis(250);

pub(crate) struct Stdin {
    /// What has been read and not yet consumed through [`Read`].
    buf: Vec<u8>,
    pos: usize,
    chunks: Receiver<io::Result<Vec<u8>>>,
    ended: bool,
}

impl Stdin {
    /// Starts reading standard input on a thread.
    pub(crate) fn spawn() -> io::Result<Self> {
        Self::spawn_from(io::stdin())
    }

    fn spawn_from(mut input: impl Read + Send + 'static) -> io::Result<Self> {
        let (tx, chunks) = mpsc::sync_channel(QUEUE);
        thread::Builder::new()
            .name("dffy-stdin".into())
            .spawn(move || {
                loop {
                    let mut chunk = vec![0; CHUNK];
                    let read = match input.read(&mut chunk) {
                        Ok(0) => return,
                        Ok(n) => {
                            chunk.truncate(n);
                            Ok(chunk)
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => Err(e),
                    };
                    let failed = read.is_err();
                    if tx.send(read).is_err() || failed {
                        return;
                    }
                }
            })?;
        Ok(Self {
            buf: Vec::new(),
            pos: 0,
            chunks,
            ended: false,
        })
    }

    /// Everything read so far and not consumed.
    pub(crate) fn buffered(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// Waits for the next chunk until `deadline` (forever without one).
    /// Returns false when the deadline passed first.
    fn fill(&mut self, deadline: Option<Instant>) -> io::Result<bool> {
        if self.ended {
            return Ok(true);
        }
        let next = match deadline {
            Some(at) => match self
                .chunks
                .recv_timeout(at.saturating_duration_since(Instant::now()))
            {
                Ok(chunk) => Some(chunk),
                Err(RecvTimeoutError::Timeout) => return Ok(false),
                Err(RecvTimeoutError::Disconnected) => None,
            },
            None => self.chunks.recv().ok(),
        };
        match next {
            Some(chunk) => self.buf.extend_from_slice(&chunk?),
            None => self.ended = true,
        }
        Ok(true)
    }
}

impl Read for Stdin {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            if self.ended {
                return Ok(0);
            }
            let Ok(chunk) = self.chunks.recv() else {
                self.ended = true;
                return Ok(0);
            };
            self.buf = chunk?;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Diff,
    NotDiff,
    /// Not enough input yet.
    Unsure,
}

/// Whether `prefix`, the start of the input, is a diff: it has a
/// `diff --git`, `diff --cc` or `diff --combined` line, or a `--- `,
/// `+++ ` pair followed by a hunk header, all at the start of a line and
/// with any colors removed. `ended` says the input stops there.
pub(crate) fn sniff(prefix: &[u8], ended: bool) -> Verdict {
    let log = is_commit_line(&first_line(prefix));
    let limit = if log { SNIFF_LOG_BYTES } else { SNIFF_BYTES };
    let mut clean = Vec::new();
    let mut lines = 0;
    // `--- ` then `+++ ` seen on the lines before.
    let mut pair = 0;
    let mut rest = prefix;
    while let Some(end) = memchr(b'\n', rest) {
        let line = &rest[..end];
        rest = &rest[end + 1..];
        strip_ansi(line, &mut clean);
        let line = clean.strip_suffix(b"\r").unwrap_or(&clean);
        if line.starts_with(b"diff --git ")
            || line.starts_with(b"diff --cc ")
            || line.starts_with(b"diff --combined ")
        {
            return Verdict::Diff;
        }
        pair = match pair {
            1 if line.starts_with(b"+++ ") => 2,
            2 if line.starts_with(b"@@ -") => return Verdict::Diff,
            _ if line.starts_with(b"--- ") => 1,
            _ => 0,
        };
        lines += 1;
        if !log && lines >= SNIFF_LINES {
            return Verdict::NotDiff;
        }
    }
    if ended || prefix.len() >= limit {
        Verdict::NotDiff
    } else {
        Verdict::Unsure
    }
}

fn first_line(bytes: &[u8]) -> Vec<u8> {
    let line = &bytes[..memchr(b'\n', bytes).unwrap_or(bytes.len())];
    let mut clean = Vec::new();
    strip_ansi(line, &mut clean);
    clean
}

fn is_commit_line(line: &[u8]) -> bool {
    line.strip_prefix(b"commit ")
        .is_some_and(|rest| rest.iter().take_while(|b| b.is_ascii_hexdigit()).count() >= 7)
}

fn memchr(byte: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == byte)
}

/// What to do with piped input on a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Not a diff: pass it on unchanged.
    NotDiff,
    /// A diff, to print.
    Print,
    /// A diff that ended and fits one screen: print it, no pager.
    Fits,
    /// A diff longer than a screen, or still arriving: open the viewer.
    Viewer,
}

/// Reads as much of `stdin` as it takes to decide. With `screen`, the
/// screen's rows and whether the layout is split, a diff that ends within
/// one screen is told apart from a longer one; without it, every diff is
/// printed.
pub(crate) fn decide(stdin: &mut Stdin, screen: Option<(usize, bool)>) -> io::Result<Decision> {
    let mut deadline = None;
    loop {
        match sniff(stdin.buffered(), stdin.ended) {
            Verdict::Diff => break,
            Verdict::NotDiff => return Ok(Decision::NotDiff),
            Verdict::Unsure => {}
        }
        if !stdin.buf.is_empty() {
            deadline.get_or_insert_with(|| Instant::now() + SNIFF_WAIT);
        }
        if !stdin.fill(deadline)? {
            return Ok(Decision::NotDiff);
        }
    }
    let Some((height, split)) = screen else {
        return Ok(Decision::Print);
    };
    let deadline = Instant::now() + FIT_WAIT;
    let mut rows = Rows::new(split);
    loop {
        rows.feed(stdin.buffered());
        if rows.at_least > height {
            return Ok(Decision::Viewer);
        }
        if stdin.ended {
            return Ok(Decision::Fits);
        }
        if !stdin.fill(Some(deadline))? {
            return Ok(Decision::Viewer);
        }
    }
}

/// A lower bound on the rows a diff renders to, from its parse alone:
/// rendering lags (a hunk is laid out once it ends), and the bound keeps
/// up with every line.
struct Rows {
    parser: Parser,
    /// Bytes of the input already fed.
    fed: usize,
    split: bool,
    at_least: usize,
    /// Changed lines since the last context line; split view pairs them.
    changed: usize,
}

impl Rows {
    fn new(split: bool) -> Self {
        Self {
            parser: Parser::default(),
            fed: 0,
            split,
            at_least: 0,
            changed: 0,
        }
    }

    /// Feeds the complete lines of `input` after those already fed.
    fn feed(&mut self, input: &[u8]) {
        let mut clean = Vec::new();
        while let Some(end) = memchr(b'\n', &input[self.fed..]) {
            let line = &input[self.fed..self.fed + end];
            self.fed += end + 1;
            let line = if line.contains(&0x1b) {
                strip_ansi(line, &mut clean);
                &clean[..]
            } else {
                line
            };
            let (split, at_least, changed) = (self.split, &mut self.at_least, &mut self.changed);
            let _ = self.parser.feed(line, &mut |event| -> Result<(), ()> {
                *at_least += match event {
                    Event::File(_) => 2,
                    Event::Line(Kind::Removed | Kind::Added, _) if split => {
                        // Pairs share a row: every second changed line
                        // adds one at least.
                        *changed += 1;
                        usize::from(*changed % 2 == 1)
                    }
                    Event::Line(..) => {
                        *changed = 0;
                        1
                    }
                    Event::Text(..) | Event::Hunk(_) | Event::Combined { .. } | Event::Stray(_) => {
                        1
                    }
                    Event::HunkEnd => 0,
                };
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(input: &[u8]) -> Verdict {
        sniff(input, true)
    }

    #[test]
    fn diffs_are_told_from_other_git_output() {
        let log = b"commit 0123456789abcdef0123456789abcdef01234567\n\
            Author: Ada <ada@example.com>\nDate:   Mon Jan 1 00:00:00 2024 +0000\n\n    \
            diff --git a/x b/x in a message\n\n";
        assert_eq!(verdict(log), Verdict::NotDiff, "git log without -p");
        let branch = b"* main\n  feature\n  diff-tool\n";
        assert_eq!(verdict(branch), Verdict::NotDiff, "git branch");
        let blame = b"0123abcd (Ada 2024-01-01 1) --- a\n0123abcd (Ada 2024-01-01 2) +++ b\n\
            0123abcd (Ada 2024-01-01 3) @@ -1 +1 @@\n";
        assert_eq!(verdict(blame), Verdict::NotDiff, "git blame");
        let fixture = |name: &str| {
            std::fs::read(format!(
                "{}/tests/fixtures/{name}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .expect("fixture")
        };
        for name in [
            "git-diff.diff",
            "git-diff-color.diff",
            "git-log-color.diff",
            "git-show.diff",
            "git-show-merge.diff",
            "diff-u.diff",
        ] {
            assert_eq!(verdict(&fixture(name)), Verdict::Diff, "{name}");
        }
    }

    #[test]
    fn a_diff_marker_needs_a_line_of_its_own_and_its_hunk() {
        assert_eq!(verdict(b"--- a\n+++ b\nno hunk\n"), Verdict::NotDiff);
        assert_eq!(verdict(b"x --- a\n+++ b\n@@ -1 +1 @@\n"), Verdict::NotDiff);
        assert_eq!(verdict(b"--- a\n+++ b\n@@ -1 +1 @@\n"), Verdict::Diff);
        assert_eq!(
            verdict(b"\x1b[1mdiff --git a/x b/x\x1b[m\n"),
            Verdict::Diff,
            "colored"
        );
    }

    #[test]
    fn short_input_waits_and_long_input_gives_up() {
        assert_eq!(sniff(b"* main\n", false), Verdict::Unsure);
        let lines = "line\n".repeat(SNIFF_LINES);
        assert_eq!(sniff(lines.as_bytes(), false), Verdict::NotDiff);
        // A log is read further for its first diff.
        let mut log = b"commit 0123456789abcdef\n\n".to_vec();
        log.extend_from_slice("    message\n".repeat(SNIFF_LINES).as_bytes());
        assert_eq!(sniff(&log, false), Verdict::Unsure);
        log.extend_from_slice(b"diff --git a/x b/x\n");
        assert_eq!(sniff(&log, false), Verdict::Diff);
    }

    #[test]
    fn short_diffs_fit_and_long_ones_open_the_viewer() {
        let decide_on = |input: &'static [u8], height| {
            let mut stdin = Stdin::spawn_from(input).expect("thread");
            decide(&mut stdin, Some((height, false))).expect("decide")
        };
        let diff = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n";
        assert_eq!(decide_on(diff, 24), Decision::Fits);
        assert_eq!(decide_on(diff, 4), Decision::Viewer);
        assert_eq!(decide_on(b"plain text\n", 24), Decision::NotDiff);
        let mut stdin = Stdin::spawn_from(&diff[..]).expect("thread");
        assert_eq!(decide(&mut stdin, None).expect("decide"), Decision::Print);
        let mut all = Vec::new();
        stdin.read_to_end(&mut all).expect("read");
        assert_eq!(all, diff, "nothing read for the decision is lost");
    }
}
