//! Finding where a name is used: a whole-word search of a navigator's side,
//! closest files first, run a few files at a time so a host can stream
//! what it finds and yield to other work between files.

use std::ops::RangeInclusive;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use gix::bstr::BString;
use rustc_hash::{FxHashMap, FxHashSet};

use super::names::{Comments, uses, uses_at};
use super::{
    Candidate, Error, MAX_SEARCH_BYTES, Navigator, Origin, Progress, Rank, Slot, differs, hash,
    is_word_byte,
};
use crate::git::{self, FileChange, Repo};
use crate::highlight::DefinitionKind;

/// What a diff changes, by path.
type Changes = FxHashMap<BString, FileChange>;

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

/// Which uses a search reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Filter {
    /// Only the uses calling the name: followed by its arguments, `(`,
    /// past a Rust macro's `!` or the type arguments a call names.
    pub calls_only: bool,
    /// Only the uses in files the diff changes.
    pub in_diff: bool,
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
    filter: Filter,
    /// Of a search in the diff alone.
    diffed: Option<Diffed>,
    /// The indexed files defining the name, as of how many files the
    /// index had: a host steps a file at a time.
    defined_in: (usize, Vec<u32>),
}

/// What a search in the diff alone picks its files from: a navigator takes a
/// diff's changes again.
#[derive(Debug)]
struct Diffed {
    /// Every file of the side, in the search's order.
    all: Vec<(Rank, u32)>,
    /// The changes they were picked by.
    by: Arc<Changes>,
}

impl Navigator {
    /// A search for the lines using `name` as a whole word at this side,
    /// the files closest to `origin` first, as [`Navigator::lookup`] ranks
    /// them. Every file is searched, whatever its language, but binary
    /// files, those over 8MB and those that can't be read. In a file of a
    /// language with a tags query, a use inside a comment (a block comment
    /// over several lines included) or a string is left out, and so is the
    /// first use inside each of the name's definitions there, which is the
    /// definition naming it, once the index has the file. `filter` keeps
    /// only some uses; a file counts when it has one it keeps. An empty
    /// name finds nothing.
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
        filter: Filter,
        cancel: &AtomicBool,
    ) -> Result<References, Error> {
        let mut order = if name.is_empty() {
            Vec::new()
        } else {
            let related = self.related(repo, origin, cancel)?;
            self.search_order(&related, origin)
        };
        let mut diffed = None;
        if filter.in_diff {
            let changes = self.changes();
            let all = order.clone();
            order.retain(|&(_, l)| self.differs(&changes, l));
            diffed = Some(Diffed { all, by: changes });
        }
        Ok(References {
            navigator: self.id,
            name: name.to_owned(),
            finder: memchr::memmem::Finder::new(name.as_bytes()).into_owned(),
            order,
            next: 0,
            files: 0,
            filter,
            diffed,
            defined_in: (usize::MAX, Vec::new()),
        })
    }

    /// Whether the diff whose `changes` these are changes listed file `l`.
    fn differs(&self, changes: &Changes, l: u32) -> bool {
        (changes.get(&self.listed[l as usize].path)).is_some_and(differs)
    }

    /// The indexed files the index has definitions of `name` in so far,
    /// ascending.
    fn defined_in(&self, name: &str) -> Vec<u32> {
        let mut state = self.state();
        state
            .posted(self.shared.store(), hash(name))
            .unwrap_or_else(|| {
                self.reindex(&mut state);
                Vec::new()
            })
    }

    /// The definitions of `name` in indexed file `file`, as far as the index
    /// has it.
    fn defining(&self, file: u32, name: &str) -> Vec<Defining> {
        let Slot::Found(Some(at)) = self.state().slots[file as usize] else {
            return Vec::new();
        };
        (self.read(at))
            .map(|defs| {
                defs.iter()
                    .filter(|d| d.name == name)
                    .map(|d| Defining {
                        lines: d.start..=d.end,
                        function: matches!(
                            d.kind,
                            DefinitionKind::Function | DefinitionKind::Method
                        ),
                    })
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
        if let Some(Diffed { all, by }) = &mut self.diffed
            && !Arc::ptr_eq(by, &in_diff)
        {
            // What's searched stays; the rest is what the diff has now.
            self.order.truncate(self.next);
            let searched: FxHashSet<u32> = self.order.iter().map(|&(_, l)| l).collect();
            self.order.extend(
                all.iter()
                    .filter(|&&(_, l)| !searched.contains(&l) && navigator.differs(&in_diff, l)),
            );
            *by = Arc::clone(&in_diff);
        }
        let indexed = navigator.progress().done;
        if self.defined_in.0 != indexed {
            self.defined_in = (indexed, navigator.defined_in(&self.name));
        }
        let defined_in = &self.defined_in.1;
        let mut out = Vec::new();
        // From the pool and back to it each step: a search stopped part way,
        // or left at its first files, holds none.
        let mut text = take_buffer();
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
            if repo.load_into(path, side, &mut text).is_err() || git::is_binary(&text) {
                continue;
            }
            let language = listed.indexed.map(|i| {
                let indexed = &navigator.indexed[i as usize];
                let language = &navigator.languages[usize::from(indexed.language)];
                // A header's is what its text reads as.
                (i, language.of_text(path, &text).name())
            });
            let file = File {
                text: &text,
                name: &self.name,
                finder: &self.finder,
                calls_only: self.filter.calls_only,
                language: language.map(|(_, name)| name),
                defining: (language.map(|(i, _)| i))
                    .filter(|i| defined_in.binary_search(i).is_ok())
                    .map_or_else(Vec::new, |i| navigator.defining(i, &self.name)),
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
        give_back(text);
        out
    }
}

/// The read buffers of searches done, for the next to take. A buffer
/// grows to the largest file its search reads; one made anew for each
/// search would leave the allocator holding each one's memory once freed.
static BUFFERS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Buffers [`BUFFERS`] keeps: as many searches as usually run at once.
const POOLED: usize = 2;

/// A read buffer for a search, from those of searches done if any.
pub(super) fn take_buffer() -> Vec<u8> {
    let mut pool = BUFFERS.lock().unwrap_or_else(PoisonError::into_inner);
    pool.pop().unwrap_or_default()
}

/// Drops the read buffers kept for the next search, giving their memory
/// back: a host calls it once searches have stopped for a while. The next
/// search grows one again.
pub fn release_search_buffers() {
    let mut pool = BUFFERS.lock().unwrap_or_else(PoisonError::into_inner);
    *pool = Vec::new();
}

/// The buffers kept, for tests.
#[cfg(test)]
pub(super) fn pooled() -> usize {
    BUFFERS.lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// Gives a search's read buffer back for the next, its room kept: given
/// back smaller, each search would grow it again through allocations the
/// allocator keeps once freed.
pub(super) fn give_back(mut buffer: Vec<u8>) {
    buffer.clear();
    let mut pool = BUFFERS.lock().unwrap_or_else(PoisonError::into_inner);
    if pool.len() < POOLED {
        pool.push(buffer);
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
    /// Only the uses calling the name count.
    calls_only: bool,
    /// The definitions of the name in it, as indexed.
    defining: Vec<Defining>,
}

/// A definition of the name searched for.
struct Defining {
    /// The lines it spans.
    lines: RangeInclusive<usize>,
    function: bool,
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
            // The part of the line that is code, past a comment or a string
            // open at its start.
            let mut code = bytes;
            if let Some(language) = self.language {
                if fed < starts {
                    comments.skip(&text[fed..starts]);
                }
                fed = (stops + 1).min(text.len());
                match comments.line(bytes) {
                    Some(part) if uses(part, language, self.name) => code = part,
                    _ => continue,
                }
            }
            if !self.counts(bytes, code, line, &mut taken) {
                continue;
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

    /// Whether line number `line`, `bytes` whose part `code` uses the name,
    /// has a use that counts: one that isn't what defines it there, and with
    /// calls alone asked for, one that calls it. A definition whose defining
    /// use is on the line is marked in `taken`.
    fn counts(&self, bytes: &[u8], code: &[u8], line: usize, taken: &mut [bool]) -> bool {
        let defined =
            |taken: &[bool], i: usize| !taken[i] && self.defining[i].lines.contains(&line);
        if !self.calls_only && !(0..taken.len()).any(|i| defined(taken, i)) {
            return true;
        }
        // Each use's part of the line before it and past it.
        let past = bytes.len() - code.len();
        let mut left: Vec<(&[u8], &[u8])> = (uses_at(code, self.language, self.name).into_iter())
            .map(|at| past + at)
            .map(|at| (&bytes[..at], &bytes[at + self.name.len()..]))
            .collect();
        for i in 0..taken.len() {
            if defined(taken, i)
                && let Some(defining) = defining_use(&left, self.defining[i].function)
            {
                taken[i] = true;
                left.remove(defining);
            }
        }
        (left.iter()).any(|(before, after)| !self.calls_only || calls(before, after))
    }
}

/// Which of a line's `uses` of a name, each by what's before and after it, is
/// the one a definition of it names it by: the one past a word that defines,
/// else the first; for a function, the first that isn't a type before the
/// name (C's `struct foo *foo(`), if any.
fn defining_use(uses: &[(&[u8], &[u8])], function: bool) -> Option<usize> {
    let by_word = uses.iter().position(|(before, _)| defines(before));
    if by_word.is_some() || !function {
        return by_word.or_else(|| (!uses.is_empty()).then_some(0));
    }
    uses.iter().position(|(_, after)| {
        let next = after.trim_ascii_start().first();
        next.is_some_and(|&b| !(is_word_byte(b) || matches!(b, b'*' | b'&')))
    })
}

/// Whether what's `before` a name on its line ends in a word that defines a
/// function of it.
fn defines(before: &[u8]) -> bool {
    let before = before.trim_ascii_end();
    [&b"fn"[..], b"def", b"func", b"fun", b"function"]
        .iter()
        .any(|k| {
            (before.strip_suffix(*k)).is_some_and(|b| b.last().is_none_or(|&b| !is_word_byte(b)))
        })
}

/// Whether a use of a name, `before` it and `after` it on its line, calls
/// it: the arguments come next, after a Rust macro's `!`, a turbofish
/// `::<…>`, or type arguments right after it, in `<…>` or Go's `[…]`. A
/// definition's own name (`fn f(`, `def f(`, `func f(`, `fun f(`,
/// `function f(`) doesn't. A line-level rule: a C declaration `int f(void);`
/// reads as a call, unless the index has it as the name's definition.
pub(super) fn calls(before: &[u8], after: &[u8]) -> bool {
    if defines(before) {
        return false;
    }
    let rest = after.trim_ascii_start();
    if let Some(rest) = rest.strip_prefix(b"!") {
        let rest = rest.trim_ascii_start();
        return matches!(rest.first(), Some(b'(' | b'[' | b'{'));
    }
    // A comparison has a space before its `<`.
    let (turbofish, arguments) = match rest.strip_prefix(b"::") {
        Some(rest) => (true, rest),
        None => (false, after),
    };
    let rest = match arguments.first() {
        Some(b'<' | b'[') => match type_arguments(arguments, turbofish) {
            Some(end) => arguments[end..].trim_ascii_start(),
            None => return false,
        },
        _ if turbofish => arguments,
        _ => rest,
    };
    rest.first() == Some(&b'(')
}

/// Where the type arguments `text` opens with close, `None` when they don't
/// on the line or hold what no type does: a `turbofish` is nothing else, so
/// it may hold a `const` argument's braces.
fn type_arguments(text: &[u8], turbofish: bool) -> Option<usize> {
    let open = *text.first()?;
    let close = if open == b'<' { b'>' } else { b']' };
    let mut depth = 0usize;
    for (i, &b) in text.iter().enumerate() {
        let next = text.get(i + 1);
        match b {
            _ if b == open => depth += 1,
            // Not the `>` of `->` or `=>`.
            _ if b == close && !(b == b'>' && matches!(text[i - 1], b'-' | b'=')) => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b';' => return None,
            b'{' if !turbofish => return None,
            b'&' | b'|' if next == Some(&b) => return None,
            b'=' if next == Some(&b'=') || (!turbofish && next != Some(&b'>')) => return None,
            _ => {}
        }
    }
    None
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

#[cfg(test)]
mod tests;
