//! `--color-only`, for git's `interactive.diffFilter`. git pairs the
//! filter's output with its input line by line (`git add -p` splits and
//! shows hunks by those lines), so each input line comes out as exactly
//! one line with the same text: escape sequences in the input are dropped,
//! colors are added around the text, and nothing else changes. Tabs and
//! control bytes stay as they are, and each line keeps its terminator.
//!
//! Lines are classified one at a time. A hunk's lines follow its `@@`
//! counts while they hold; past them (a hand-edited hunk with a stale
//! header), lines keep the color their prefix gives until the next hunk,
//! file or commit starts. A wrong guess costs a color, never text.
//!
//! Code isn't syntax-highlighted: hunk fragments parse poorly without
//! their file, and compiling one language's query costs up to about
//! 140ms (Swift's).

use std::io::{self, BufRead, BufReader, Read, Write};

use dffy_core::diff::pair_lines;
use dffy_core::patch::{self, strip_ansi};
use dffy_core::view::Kind;

use super::{MAX_BLOCK_LINES, combined_role};
use crate::theme::{Role, Theme};

#[derive(Debug, Clone, Copy)]
enum State {
    /// Commit headers, messages and other text.
    Text,
    /// A file's header lines, before its first hunk.
    Header,
    /// Lines the hunk's header still counts on each side; both 0 once
    /// they're used up.
    Hunk {
        old: usize,
        new: usize,
    },
    Combined {
        parents: usize,
    },
    /// A `GIT binary patch` body.
    Binary,
}

/// What an input line is.
enum Class {
    Role(Role),
    Line(Kind),
    /// `--- ` where it may start a file (`diff -u` output) or be a removed
    /// line: the next line tells.
    Minus,
}

/// A line held back: a changed line waiting for its pair, or a `Minus`.
struct Held {
    text: Vec<u8>,
    end: &'static [u8],
    kind: Kind,
}

/// Copies the diff on `input` to `out` a line at a time, colored with
/// `theme` (none: only escape sequences are removed). With `tinted`,
/// changed lines take the background tints of `indicators = background`.
pub(crate) fn run(
    input: impl Read,
    out: impl Write,
    theme: Option<&Theme>,
    tinted: bool,
) -> io::Result<()> {
    let mut colorer = Colorer {
        out,
        theme,
        tinted,
        state: State::Text,
        commit_meta: false,
        block: Vec::new(),
        minus: None,
    };
    let mut input = BufReader::with_capacity(64 << 10, input);
    let mut raw = Vec::with_capacity(256);
    let mut clean = Vec::with_capacity(256);
    loop {
        raw.clear();
        if input.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        let (body, newline) = match raw.strip_suffix(b"\n") {
            Some(body) => (body, true),
            None => (&raw[..], false),
        };
        strip_ansi(body, &mut clean);
        let cr = clean.last() == Some(&b'\r');
        if cr {
            clean.pop();
        }
        let end: &'static [u8] = match (cr, newline) {
            (true, true) => b"\r\n",
            (false, true) => b"\n",
            (true, false) => b"\r",
            (false, false) => b"",
        };
        colorer.line(&clean, end)?;
    }
    colorer.finish()?;
    colorer.out.flush()
}

struct Colorer<'t, W> {
    out: W,
    theme: Option<&'t Theme>,
    tinted: bool,
    state: State,
    commit_meta: bool,
    /// The current change block's removed and added lines, held for
    /// pairing, with any no-newline markers among them.
    block: Vec<Held>,
    minus: Option<Held>,
}

impl<W: Write> Colorer<'_, W> {
    fn line(&mut self, text: &[u8], end: &'static [u8]) -> io::Result<()> {
        if let Some(minus) = self.minus.take() {
            if text.starts_with(b"+++ ") {
                self.flush_block()?;
                self.state = State::Header;
                self.write(Role::FileHeader, &minus.text, minus.end)?;
                return self.write(Role::FileHeader, text, end);
            }
            if matches!(self.state, State::Hunk { .. }) {
                self.hold(minus)?;
            } else {
                self.write(Role::Plain, &minus.text, minus.end)?;
            }
        }
        match self.classify(text) {
            Class::Line(Kind::Context) => {
                self.flush_block()?;
                self.write(Role::Context, text, end)
            }
            Class::Line(kind) => self.hold(Held {
                text: text.to_vec(),
                end,
                kind,
            }),
            Class::Role(role) => {
                self.flush_block()?;
                self.write(role, text, end)
            }
            Class::Minus => {
                self.minus = Some(Held {
                    text: text.to_vec(),
                    end,
                    kind: Kind::Removed,
                });
                Ok(())
            }
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        if let Some(minus) = self.minus.take() {
            if matches!(self.state, State::Hunk { .. }) {
                self.hold(minus)?;
            } else {
                self.flush_block()?;
                self.write(Role::Plain, &minus.text, minus.end)?;
            }
        }
        self.flush_block()
    }

    fn classify(&mut self, line: &[u8]) -> Class {
        match self.state {
            State::Hunk { old, new } => {
                let counted = match line.first() {
                    Some(b' ') | None if old > 0 && new > 0 => {
                        Some((Kind::Context, old - 1, new - 1))
                    }
                    Some(b'-') if old > 0 => Some((Kind::Removed, old - 1, new)),
                    Some(b'+') if new > 0 => Some((Kind::Added, old, new - 1)),
                    Some(b'\\') => Some((Kind::NoNewline, old, new)),
                    _ => None,
                };
                if let Some((kind, old, new)) = counted {
                    self.state = State::Hunk { old, new };
                    return Class::Line(kind);
                }
                if !starts_section(line) {
                    match line.first() {
                        Some(b'+') => return Class::Line(Kind::Added),
                        Some(b'-') if line.starts_with(b"--- ") => return Class::Minus,
                        Some(b'-') => return Class::Line(Kind::Removed),
                        Some(b' ') | None => return Class::Line(Kind::Context),
                        _ => {}
                    }
                }
                self.state = State::Text;
            }
            State::Combined { parents } => {
                if line.starts_with(b"@@@") {
                    let parents = line.iter().take_while(|&&b| b == b'@').count() - 1;
                    self.state = State::Combined { parents };
                    return Class::Role(Role::HunkHeader);
                }
                if !starts_section(line) {
                    return Class::Role(combined_role(patch::combined_kind(parents, line)));
                }
                self.state = State::Text;
            }
            State::Binary => {
                if !starts_section(line) {
                    return Class::Role(Role::Dim);
                }
                self.state = State::Text;
            }
            State::Header | State::Text => {}
        }
        if line.starts_with(b"@@ ") {
            let (old, new) =
                patch::parse_hunk_header(line).map_or((0, 0), |h| (h.old_len, h.new_len));
            self.state = State::Hunk { old, new };
            return Class::Role(Role::HunkHeader);
        }
        if line.starts_with(b"diff --cc ") || line.starts_with(b"diff --combined ") {
            self.state = State::Combined { parents: 0 };
            return Class::Role(Role::FileHeader);
        }
        if line.starts_with(b"diff --git ") {
            self.state = State::Header;
            return Class::Role(Role::FileHeader);
        }
        if line.starts_with(b"diff ") {
            self.state = State::Header;
            return Class::Role(Role::Dim);
        }
        if patch::is_commit_header(line) {
            self.state = State::Text;
            self.commit_meta = true;
            return Class::Role(Role::CommitHeader);
        }
        if let State::Header = self.state {
            return if line.starts_with(b"--- ") || line.starts_with(b"+++ ") {
                Class::Role(Role::FileHeader)
            } else {
                if line == b"GIT binary patch" {
                    self.state = State::Binary;
                }
                // `index`, modes, renames, `Binary files ... differ`.
                Class::Role(Role::Dim)
            };
        }
        if line.starts_with(b"--- ") {
            return Class::Minus;
        }
        if line.is_empty() {
            self.commit_meta = false;
        } else if self.commit_meta && patch::is_meta_line(line) {
            return Class::Role(Role::Dim);
        }
        Class::Role(Role::Plain)
    }

    fn hold(&mut self, line: Held) -> io::Result<()> {
        self.block.push(line);
        if self.block.len() >= MAX_BLOCK_LINES {
            self.flush_block()?;
        }
        Ok(())
    }

    fn flush_block(&mut self) -> io::Result<()> {
        let block = std::mem::take(&mut self.block);
        let emphasis = pair_lines(block.len(), |i| {
            (block[i].kind, block[i].text.get(1..).unwrap_or_default())
        });
        // Paired lines stay red and green here: this output stands in for
        // git's own, line for line.
        for (line, (emphasis, _)) in block.iter().zip(&emphasis) {
            let (base, strong) = match line.kind {
                Kind::Added if self.tinted => (Role::AddedLine, Role::AddedEmph),
                Kind::Added => (Role::Added, Role::AddedEmph),
                Kind::Removed if self.tinted => (Role::RemovedLine, Role::RemovedEmph),
                Kind::Removed => (Role::Removed, Role::RemovedEmph),
                Kind::Context => (Role::Context, Role::Context),
                Kind::NoNewline => (Role::Dim, Role::Dim),
            };
            let text = &line.text;
            let mut at = 0;
            let mut on = self.set(None, base)?;
            for range in emphasis {
                // The ranges are within the text after the sign.
                let (start, stop) = (
                    (range.start + 1).min(text.len()),
                    (range.end + 1).min(text.len()),
                );
                self.out.write_all(&text[at..start])?;
                on = self.set(on, strong)?;
                self.out.write_all(&text[start..stop])?;
                on = self.set(on, base)?;
                at = stop;
            }
            self.out.write_all(&text[at..])?;
            self.end(on, line.end)?;
        }
        Ok(())
    }

    fn write(&mut self, role: Role, text: &[u8], end: &[u8]) -> io::Result<()> {
        let on = self.set(None, role)?;
        self.out.write_all(text)?;
        self.end(on, end)
    }

    /// Switches to `role`'s style from `on`, the style set before (`None`
    /// for the terminal's default). Returns the style now set.
    fn set(&mut self, on: Option<Role>, role: Role) -> io::Result<Option<Role>> {
        let sgr = self.theme.map_or("", |t| t.sgr(role.into()));
        if sgr.is_empty() {
            if on.is_some() {
                self.out.write_all(b"\x1b[m")?;
            }
            return Ok(None);
        }
        if on != Some(role) {
            self.out.write_all(sgr.as_bytes())?;
        }
        Ok(Some(role))
    }

    fn end(&mut self, on: Option<Role>, end: &[u8]) -> io::Result<()> {
        if on.is_some() {
            self.out.write_all(b"\x1b[m")?;
        }
        self.out.write_all(end)
    }
}

/// Whether `line` starts a hunk, a file or a commit.
fn starts_section(line: &[u8]) -> bool {
    line.starts_with(b"@@ ") || line.starts_with(b"diff ") || patch::is_commit_header(line)
}

#[cfg(test)]
mod tests;
