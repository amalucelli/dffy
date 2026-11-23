//! Finding where a name is used: a whole-word search of a navigator's side,
//! closest files first, run a few files at a time so a host can stream
//! what it finds and yield to other work between files.

use std::ops::RangeInclusive;
use std::sync::atomic::AtomicBool;

use super::names::{Comments, uses};
use super::{
    Candidate, Error, MAX_SEARCH_BYTES, Navigator, Origin, Progress, Rank, Slot, differs, hash,
    is_word_byte,
};
use crate::git::{self, Repo};
use crate::highlight::Definitions;

/// The files a search reports the uses of; past them, it only counts files.
pub const MAX_REFERENCE_FILES: usize = 200;

/// The uses of one file a search reports; past them, it counts them.
pub const MAX_REFERENCE_LINES: usize = 1000;

/// Bytes of a use's line kept, past which it's cut.
const LINE_BYTES: usize = 200;

/// A line using the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Where it is: a candidate without a kind.
    pub candidate: Candidate,
    /// The line, trimmed, at most 200 bytes of it.
    pub text: String,
    /// On a file's last line reported, its uses past
    /// [`MAX_REFERENCE_LINES`], not reported; else 0.
    pub more: usize,
}

/// A search for a name's uses at a navigator's side, from
/// [`Navigator::references`]. Each [`References::step`] searches the next
/// files; the lines of the first [`MAX_REFERENCE_FILES`] files with a use
/// are handed out, and the rest are counted.
#[derive(Debug)]
pub struct References {
    /// The navigator it searches, the only one it steps with.
    navigator: u64,
    name: String,
    finder: memchr::memmem::Finder<'static>,
    /// The side's files, closest first, and the next to search.
    order: Vec<(Rank, u32)>,
    next: usize,
    /// Files with a use found so far.
    files: usize,
}

impl Navigator {
    /// A search for the lines using `name` as a whole word at this side,
    /// the files closest to `origin` first, as [`Navigator::lookup`] ranks
    /// them. Every file is searched, whatever its language, but binary
    /// files, those over 8MB and those that can't be read. In a file of a
    /// language with a tags query, a use inside a comment (a block comment
    /// over several lines included) or a string is left out, and so is the
    /// first use inside each of the name's definitions there, which is the
    /// definition naming it, once the index has the file. An empty name
    /// finds nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Cancelled`] when cancelled while reading what `origin`
    /// includes; when the repository can't be read.
    pub fn references(
        &self,
        repo: &Repo,
        name: &str,
        origin: &Origin,
        cancel: &AtomicBool,
    ) -> Result<References, Error> {
        let order = if name.is_empty() {
            Vec::new()
        } else {
            let related = self.related(repo, origin, cancel)?;
            self.search_order(&related, origin)
        };
        Ok(References {
            navigator: self.id,
            name: name.to_owned(),
            finder: memchr::memmem::Finder::new(name.as_bytes()).into_owned(),
            order,
            next: 0,
            files: 0,
        })
    }

    /// The lines the definitions of `name` in indexed file `file` span, as
    /// far as the index has it.
    fn defining(&self, file: u32, name: &str) -> Vec<RangeInclusive<usize>> {
        let entry = match &self.state().slots[file as usize] {
            Slot::Found(Some(entry)) if entry.has(hash(name)) => std::sync::Arc::clone(entry),
            _ => return Vec::new(),
        };
        (Definitions::decode(&entry.encoded))
            .map(|defs| {
                defs.iter()
                    .filter(|d| d.name == name)
                    .map(|d| d.start..=d.end)
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl References {
    /// The name searched for.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Files with a use found so far.
    #[must_use]
    pub fn files(&self) -> usize {
        self.files
    }

    /// How many of the side's files are searched.
    #[must_use]
    pub fn searched(&self) -> Progress {
        Progress {
            done: self.next,
            total: self.order.len(),
        }
    }

    /// Every file is searched.
    #[must_use]
    pub fn done(&self) -> bool {
        self.next >= self.order.len()
    }

    /// Searches the next files of `navigator`, the one this search came
    /// from, until every file is or `stop` says so, asked before each.
    /// Returns the uses found in files among the first
    /// [`MAX_REFERENCE_FILES`] with one, in order. With another navigator
    /// it finds nothing, and is done.
    pub fn step(
        &mut self,
        navigator: &Navigator,
        repo: &Repo,
        mut stop: impl FnMut() -> bool,
    ) -> Vec<Reference> {
        if navigator.id != self.navigator {
            self.next = self.order.len();
            return Vec::new();
        }
        let in_diff = navigator.changes();
        let mut out = Vec::new();
        while let Some(&(rank, l)) = self.order.get(self.next) {
            if stop() {
                break;
            }
            self.next += 1;
            let listed = &navigator.listed[l as usize];
            let (path, side) = (listed.path.as_ref(), &listed.side);
            if repo
                .size(path, side)
                .is_ok_and(|size| size > MAX_SEARCH_BYTES)
            {
                continue;
            }
            let Ok(text) = repo.load(path, side) else {
                continue;
            };
            if git::is_binary(&text) {
                continue;
            }
            let language = listed.indexed.map(|i| {
                let indexed = &navigator.indexed[i as usize];
                (i, navigator.languages[usize::from(indexed.language)].name())
            });
            let file = File {
                text: &text,
                name: &self.name,
                finder: &self.finder,
                language: language.map(|(_, name)| name),
                defining: language
                    .map_or_else(Vec::new, |(i, _)| navigator.defining(i, &self.name)),
            };
            let reported = self.files < MAX_REFERENCE_FILES;
            let changed = in_diff.get(&listed.path).is_some_and(differs);
            let (found, more) = file.uses(reported, |line, bytes| {
                out.push(Reference {
                    candidate: Candidate {
                        path: listed.path.clone(),
                        side: *side,
                        kind: None,
                        start: line,
                        end: line,
                        container: None,
                        rank,
                        changed,
                        in_hunk: None,
                    },
                    text: shortened(bytes),
                    more: 0,
                });
            });
            if more > 0
                && let Some(last) = out.last_mut()
            {
                last.more = more;
            }
            self.files += usize::from(found);
        }
        out
    }
}

/// A file being searched for a name's uses.
struct File<'a> {
    text: &'a [u8],
    name: &'a str,
    finder: &'a memchr::memmem::Finder<'static>,
    /// Its language, when it has a tags query: comments and strings are
    /// told apart.
    language: Option<&'a str>,
    /// The lines each definition of the name in it spans, as indexed.
    defining: Vec<RangeInclusive<usize>>,
}

impl File<'_> {
    /// Hands each line using the name to `use_at`, its number and bytes,
    /// up to [`MAX_REFERENCE_LINES`]; with `reported` false, it stops at the
    /// first. Returns whether there was one, and how many more past the
    /// cap.
    fn uses(&self, reported: bool, mut use_at: impl FnMut(usize, &[u8])) -> (bool, usize) {
        let (text, word) = (self.text, self.name.as_bytes());
        let mut comments = Comments::new(self.language.unwrap_or_default());
        // The line weighed last: where it starts, its number and where it
        // stops; and the bytes the comments have gone through.
        let (mut starts, mut line, mut stops, mut fed) = (0, 0, 0, 0);
        let mut taken = vec![false; self.defining.len()];
        let (mut listed, mut more) = (0, 0);
        for at in self.finder.find_iter(text) {
            let end = at + word.len();
            // A line is weighed once, whatever its other uses.
            if (at > 0 && is_word_byte(text[at - 1]))
                || text.get(end).is_some_and(|&b| is_word_byte(b))
                || at < stops
            {
                continue;
            }
            let line_start =
                memchr::memrchr(b'\n', &text[starts..at]).map_or(starts, |i| starts + i + 1);
            line += memchr::memchr_iter(b'\n', &text[starts..line_start]).count();
            starts = line_start;
            stops = memchr::memchr(b'\n', &text[at..]).map_or(text.len(), |i| at + i);
            let bytes = &text[starts..stops];
            if let Some(language) = self.language {
                if fed < starts {
                    comments.skip(&text[fed..starts]);
                }
                fed = (stops + 1).min(text.len());
                let code = comments.line(bytes);
                if !code.is_some_and(|code| uses(code, language, self.name)) {
                    continue;
                }
                if let Some(i) = (self.defining.iter().zip(&taken))
                    .position(|(span, taken)| !taken && span.contains(&line))
                {
                    taken[i] = true;
                    continue;
                }
            }
            if !reported {
                return (true, 0);
            }
            if listed < MAX_REFERENCE_LINES {
                use_at(line, bytes);
                listed += 1;
            } else {
                more += 1;
            }
        }
        (listed + more > 0, more)
    }
}

/// `line` trimmed, cut to [`LINE_BYTES`] at a character boundary.
fn shortened(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line.trim_ascii());
    let mut end = text.len().min(LINE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
