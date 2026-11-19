//! The comments panel's list as a model: every thread by file, which
//! threads a filter and a search keep, in what order, and which is
//! selected. A frontend scrolls and draws the rows.
//!
//! The list is made when the panel opens, and again only when the comments
//! or the list of files change; its rows when the filter or the search
//! change too.

#[cfg(test)]
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use super::model::{At, Comments, Hint, Part, Standing, Thread};
use crate::fuzzy;
use crate::review::{self, Comment, who};

#[cfg(test)]
thread_local! {
    static LISTED: Cell<usize> = const { Cell::new(0) };
}

/// Times the comments panel's list was made on this thread, to tell that
/// it isn't while the panel shows the files.
#[cfg(test)]
pub(crate) fn listed_count() -> usize {
    LISTED.with(Cell::get)
}

/// The filters in the order `f` steps through them, as the title names
/// them, and what each says when it keeps nothing.
const FILTERS: [(&str, &str); 4] = [
    ("", "no threads"),
    ("open", "no open threads"),
    ("drafts", "no drafts"),
    ("resolved", "no resolved threads"),
];

/// How pressing a thread is, most first: the order of a file's threads in
/// the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Rank {
    Draft,
    Open,
    Outdated,
    Resolved,
}

impl Rank {
    /// A thread with a draft in it is a draft, even resolved: posting
    /// sends it.
    fn of(thread: &Thread) -> Self {
        if thread
            .comments
            .iter()
            .any(|n| n.standing == Standing::Draft)
        {
            Self::Draft
        } else if thread.resolved {
            Self::Resolved
        } else if thread.outdated {
            Self::Outdated
        } else {
            Self::Open
        }
    }
}

/// Every thread the panel lists, by file, made from one generation of the
/// comments.
pub(crate) struct Listing {
    /// That generation, and whether the list of files was still growing.
    key: (u64, bool),
    pub files: Vec<Group>,
    /// File by file, in [`Listing::files`]' order.
    pub items: Vec<Item>,
    /// Threads, and open ones; general comments aren't threads.
    pub counts: [usize; 2],
    /// Commented files still to be placed.
    waiting: usize,
}

pub(crate) struct Group {
    pub path: String,
    /// Not in the list of files.
    pub absent: bool,
    /// The general comments, listed first: not a file's.
    pub conversation: bool,
    /// The threads on other branches, listed last while asked for.
    pub branches: bool,
}

pub(crate) struct Item {
    /// Its file, in [`Listing::files`].
    pub group: usize,
    pub rank: Rank,
    /// Its file's threads, and which it is.
    threads: Arc<[Thread]>,
    at: usize,
    /// Its first line, 1-based; `None` on the whole file.
    pub line: Option<usize>,
}

impl Item {
    pub(crate) fn thread(&self) -> &Thread {
        &self.threads[self.at]
    }
}

/// A row of the panel's list.
pub(crate) enum Row {
    /// A file's header, with how many of its threads are kept.
    File {
        group: usize,
        threads: usize,
    },
    /// A thread, by its index in [`Listing::items`]; `author` when the row
    /// names its author, who isn't the row above's.
    Thread {
        item: usize,
        author: bool,
    },
    Note(Vec<Part>),
}

impl Comments {
    /// Every thread's first comment, those on files not in the diff
    /// included, and how many commented files are still to be placed;
    /// `listing` while the list of files is still growing.
    fn listed(&self, listing: bool) -> (Vec<&Comment>, usize) {
        let mut out = Vec::new();
        let mut waiting: Vec<&str> = Vec::new();
        // General comments are the conversation's, listed apart.
        for root in review::threads(&self.review.comments)
            .into_iter()
            .filter(|c| c.in_reply_to.is_none() && !c.path.is_empty())
        {
            let path = root.path.as_bytes();
            let local = self.from_local.contains(&root.id);
            if local && self.located.contains_key(path) && !self.placed.contains_key(path) {
                waiting.push(&root.path);
            } else if self.hides(root) {
                // A local draft that isn't on this diff is the local
                // review's, not this one's.
            } else if root.range().is_none()
                || self.placed.contains_key(path)
                || (!self.located.contains_key(path) && !listing)
            {
                out.push(root);
            } else {
                waiting.push(&root.path);
            }
        }
        waiting.sort_unstable();
        waiting.dedup();
        (out, waiting.len())
    }

    /// [`Comments::listed`] by file: files in the list's order, then those
    /// not in it by path; a file's threads most pressing first, then by
    /// line. Placed threads are shared with the diff; threads on a file
    /// that isn't placed are made here, their code not highlighted.
    fn listing(&self, listing: bool) -> Listing {
        let (list, waiting) = self.listed(listing);
        let mut by_path: HashMap<&str, Vec<&Comment>> = HashMap::new();
        for &root in &list {
            by_path.entry(self.home(root)).or_default().push(root);
        }
        let mut paths: Vec<&str> = by_path.keys().copied().collect();
        paths.sort_unstable_by_key(|&p| (self.file_of(p.as_bytes()).unwrap_or(usize::MAX), p));
        let mut files = Vec::with_capacity(paths.len() + 1);
        let mut items = Vec::with_capacity(list.len());
        if self.conversation_len() > 0 {
            let threads = self.conversation();
            items.extend((0..threads.len()).map(|at| Item {
                group: 0,
                rank: Rank::of(&threads[at]),
                threads: Arc::clone(&threads),
                at,
                line: None,
            }));
            files.push(Group {
                path: String::new(),
                absent: false,
                conversation: true,
                branches: false,
            });
        }
        let general = items.len();
        for path in paths {
            let roots = &by_path[path];
            let absent = !self.located.contains_key(path.as_bytes());
            let threads: Arc<[Thread]> = match self.placed.get(path.as_bytes()) {
                Some(placed) => Arc::clone(&placed.threads),
                None => roots
                    .iter()
                    .map(|root| {
                        let at = if root.range().is_some() {
                            At::Lost
                        } else {
                            At::File
                        };
                        self.thread_of(root, at, None)
                    })
                    .collect(),
            };
            let start = items.len();
            for root in roots {
                let Some(at) = threads.iter().position(|t| t.root == root.id) else {
                    continue;
                };
                let line = match (&threads[at].at, root.range()) {
                    (At::Line { lines, .. }, _) => Some(lines.start + 1),
                    (_, Some((_, lines))) => Some(lines.start + 1),
                    (_, None) => None,
                };
                items.push(Item {
                    group: files.len(),
                    rank: Rank::of(&threads[at]),
                    threads: Arc::clone(&threads),
                    at,
                    line,
                });
            }
            items[start..].sort_by_key(|i| (i.rank, i.line.unwrap_or(0)));
            files.push(Group {
                path: path.to_owned(),
                absent,
                conversation: false,
                branches: false,
            });
        }
        #[cfg(test)]
        LISTED.with(|n| n.set(n.get() + 1));
        let threads = &items[general..];
        let counts = [
            threads.len(),
            threads.iter().filter(|i| !i.thread().resolved).count(),
        ];
        let elsewhere: Arc<[Thread]> = self.elsewhere_threads().into();
        if !elsewhere.is_empty() {
            items.extend((0..elsewhere.len()).map(|at| Item {
                group: files.len(),
                rank: Rank::of(&elsewhere[at]),
                threads: Arc::clone(&elsewhere),
                at,
                line: None,
            }));
            files.push(Group {
                path: String::new(),
                absent: true,
                conversation: false,
                branches: true,
            });
        }
        Listing {
            key: (self.generation, listing),
            counts,
            files,
            items,
            waiting,
        }
    }
}

/// The rows of `list` that filter `filter` and `search` keep: each file's
/// header, then its threads; the threads on other branches last, with
/// `others`, else a row saying how many there are.
fn rows(list: &Listing, filter: usize, search: &str, others: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    let note = |text: String| Row::Note(vec![Part::Text(text)]);
    if list.waiting > 0 {
        let s = if list.waiting == 1 { "" } else { "s" };
        rows.push(note(format!(
            "finding the lines of comments in {} file{s}…",
            list.waiting
        )));
    }
    if list.items.is_empty() {
        if list.waiting == 0 {
            rows.push(Row::Note(vec![
                Part::Text("no comments yet:".to_owned()),
                Part::Hint(Hint::Comment),
            ]));
        }
        return rows;
    }
    let elsewhere = |i: usize| list.files[list.items[i].group].branches;
    let folded = if others {
        0
    } else {
        (0..list.items.len()).filter(|&i| elsewhere(i)).count()
    };
    let kept: Vec<usize> = (0..list.items.len())
        .filter(|&i| (others || !elsewhere(i)) && keeps(list, &list.items[i], filter, search))
        .collect();
    if folded > 0 {
        let s = if folded == 1 { "" } else { "s" };
        rows.push(Row::Note(vec![
            Part::Text(format!("{folded} thread{s} on other branches")),
            Part::Hint(Hint::OtherBranches),
        ]));
    }
    if kept.is_empty() {
        rows.push(note(if search.is_empty() {
            FILTERS[filter].1.to_owned()
        } else {
            format!("no thread matches /{search}")
        }));
    }
    for threads in kept.chunk_by(|&a, &b| list.items[a].group == list.items[b].group) {
        rows.push(Row::File {
            group: list.items[threads[0]].group,
            threads: threads.len(),
        });
        let mut above = None;
        for &item in threads {
            let author = list.items[item]
                .thread()
                .comments
                .first()
                .map(|n| &n.author);
            rows.push(Row::Thread {
                item,
                author: author != above,
            });
            above = author;
        }
    }
    rows
}

/// Whether filter `filter` and `search` keep `item`: the search finds its
/// file's path, fuzzily, or what a comment's author or text has.
fn keeps(list: &Listing, item: &Item, filter: usize, search: &str) -> bool {
    let thread = item.thread();
    let state = match filter {
        // The filters are about threads, which general comments aren't.
        _ if list.files[item.group].conversation => filter == 0,
        1 => !thread.resolved,
        2 => item.rank == Rank::Draft,
        3 => thread.resolved,
        _ => true,
    };
    state
        && (search.is_empty()
            || fuzzy::score_close(search, &list.files[item.group].path).is_some()
            || thread.comments.iter().any(|n| {
                who(&n.author).to_lowercase().contains(search)
                    || n.text.to_lowercase().contains(search)
            }))
}

/// The comments panel's list: the threads listed, and the rows the filter,
/// the search and `others` keep of them. Which row is selected is the
/// frontend's; [`List::keep`] says where a selection goes when the rows
/// are made again.
pub(crate) struct List {
    /// Which of [`FILTERS`].
    filter: usize,
    /// Lowercase.
    search: String,
    /// The threads on other branches are listed; folded to one row
    /// otherwise.
    others: bool,
    listing: Option<Listing>,
    rows: Vec<Row>,
    /// The filter, the search or `others` changed since `rows` were made.
    stale: bool,
}

impl List {
    /// The list made, `growing` while the list of files is still growing.
    pub(crate) fn new(comments: &Comments, growing: bool) -> Self {
        let mut list = Self {
            filter: 0,
            search: String::new(),
            others: false,
            listing: None,
            rows: Vec::new(),
            stale: true,
        };
        list.sync(comments, growing);
        list
    }

    /// The row of the thread with root `near.0`, else of the first of file
    /// `near.1`'s.
    pub(crate) fn near(&self, near: (Option<&str>, &[u8])) -> Option<usize> {
        let listing = self.listing.as_ref()?;
        self.rows.iter().position(|r| match r {
            Row::Thread { item, .. } => {
                let item = &listing.items[*item];
                near.0.map_or_else(
                    || listing.files[item.group].path.as_bytes() == near.1,
                    |root| item.thread().root == root,
                )
            }
            _ => false,
        })
    }

    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The threads listed, once made.
    pub(crate) fn listing(&self) -> Option<&Listing> {
        self.listing.as_ref()
    }

    /// The thread on row `row`, as the list was last made: placed, or made
    /// for the panel when its file isn't in the diff.
    pub(crate) fn thread_at(&self, row: usize) -> Option<&Thread> {
        let Row::Thread { item, .. } = self.rows.get(row)? else {
            return None;
        };
        Some(self.listing.as_ref()?.items[*item].thread())
    }

    /// The row of the thread with root `root`, when it's kept.
    pub(crate) fn row_of(&self, root: &str) -> Option<usize> {
        (0..self.rows.len()).find(|&row| self.thread_at(row).is_some_and(|t| t.root == root))
    }

    /// The thread row `by` threads from row `from`, over the headers,
    /// stopping at either end; `from` when it isn't a thread's.
    pub(crate) fn step(&self, from: usize, by: isize) -> usize {
        let threads: Vec<usize> = (self.rows.iter().enumerate())
            .filter(|(_, r)| matches!(r, Row::Thread { .. }))
            .map(|(i, _)| i)
            .collect();
        match threads.iter().position(|&i| i == from) {
            Some(at) => threads[at.saturating_add_signed(by).min(threads.len() - 1)],
            None => from,
        }
    }

    /// Where a selection goes once the rows were made again: to the thread
    /// with root `root` when it's still kept, else to the next thread from
    /// row `at`, else to the last.
    pub(crate) fn keep(&self, root: Option<&str>, at: usize) -> usize {
        let threads: Vec<(usize, &str)> = (0..self.rows.len())
            .filter_map(|row| Some((row, self.thread_at(row)?.root.as_str())))
            .collect();
        let next = threads
            .iter()
            .find(|(_, r)| Some(*r) == root)
            .or_else(|| threads.iter().find(|&&(i, _)| i >= at))
            .or_else(|| threads.last());
        next.map_or(0, |&(i, _)| i)
    }

    /// The next filter, or with `back` the one before.
    pub(crate) fn step_filter(&mut self, back: bool) {
        let by = if back { FILTERS.len() - 1 } else { 1 };
        self.filter = (self.filter + by) % FILTERS.len();
        self.stale = true;
    }

    /// Lists the threads on other branches, or folds them.
    pub(crate) fn toggle_others(&mut self) {
        self.others = !self.others;
        self.stale = true;
    }

    pub(crate) fn search(&self) -> &str {
        &self.search
    }

    pub(crate) fn push_search(&mut self, c: char) {
        self.search.extend(c.to_lowercase());
        self.stale = true;
    }

    pub(crate) fn pop_search(&mut self) {
        self.search.pop();
        self.stale = true;
    }

    pub(crate) fn clear_search(&mut self) {
        self.search.clear();
        self.stale = true;
    }

    /// The filter's name, empty for none.
    pub(crate) fn filter_name(&self) -> &'static str {
        FILTERS[self.filter].0
    }

    /// The threads the filter and the search keep, general comments and
    /// other branches' left out.
    pub(crate) fn kept(&self) -> usize {
        let Some(listing) = &self.listing else {
            return 0;
        };
        self.rows
            .iter()
            .filter(|r| {
                matches!(r, Row::Thread { item, .. }
                    if !listing.files[listing.items[*item].group].conversation
                        && !listing.files[listing.items[*item].group].branches)
            })
            .count()
    }

    /// Whether the filter and the search keep every thread.
    pub(crate) fn unfiltered(&self) -> bool {
        self.filter == 0 && self.search.is_empty()
    }

    /// Makes the list again when the comments or the list of files
    /// changed, and the rows when the filter or the search did too.
    /// Returns whether the rows were made again.
    pub(crate) fn sync(&mut self, comments: &Comments, growing: bool) -> bool {
        let key = (comments.generation, growing);
        let old = self.listing.as_ref().is_none_or(|l| l.key != key);
        if !old && !self.stale {
            return false;
        }
        if old {
            self.listing = Some(comments.listing(growing));
        }
        self.stale = false;
        let Some(listing) = &self.listing else {
            return false;
        };
        self.rows = rows(listing, self.filter, &self.search, self.others);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A comments file of `json`'s comments, read; a review session's with
    /// `session`, on branch `main`.
    fn comments(dir: &tempfile::TempDir, json: &str, session: bool) -> Comments {
        let path = dir.path().join("comments.json");
        std::fs::write(&path, json).expect("write");
        let mut comments = Comments::new(path, None, session, Some("main".to_owned()));
        comments.reload().expect("read");
        comments
    }

    const THREADS: &str = r#"{"version":1,"comments":[
{"id":"a1","path":"src/a.rs","side":"new","start":{"old":null,"new":9},"end":{"old":null,"new":9},"text":"Overflow here","author":"ana","created":"2026-11-17T10:00:00Z","ref":"h:1"},
{"id":"a2","path":"src/a.rs","text":"Fixed","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"a1"},
{"id":"a3","path":"src/a.rs","side":"new","start":{"old":null,"new":2},"end":{"old":null,"new":2},"text":"Old news","author":"bo","created":"2026-11-17T10:00:00Z","ref":"h:2","resolved":true},
{"id":"b1","path":"lib/b.rs","text":"Whole file","author":"bo","created":"2026-11-17T10:00:00Z","ref":"h:3"}
]}"#;

    fn roots(list: &List) -> Vec<&str> {
        let listing = list.listing().expect("made");
        (list.rows().iter())
            .filter_map(|r| match r {
                Row::Thread { item, .. } => Some(listing.items[*item].thread().root.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Files by path when none is in the diff; a file's threads most
    /// pressing first: the one with your draft reply, then by line, the
    /// resolved one last. Filters keep open, draft or resolved threads, and
    /// the search a path fuzzily or what a comment says.
    #[test]
    fn filters_and_the_search_keep_threads_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let comments = comments(&dir, THREADS, true);
        let mut list = List::new(&comments, false);
        assert_eq!(roots(&list), ["b1", "a1", "a3"]);
        assert_eq!(list.listing().expect("made").counts, [3, 2]);
        assert!(list.unfiltered());
        let kept = |list: &mut List| {
            list.sync(&comments, false);
            (list.filter_name(), roots(list).join(" "), list.kept())
        };
        list.step_filter(false);
        assert_eq!(kept(&mut list), ("open", "b1 a1".to_owned(), 2));
        list.step_filter(false);
        assert_eq!(kept(&mut list), ("drafts", "a1".to_owned(), 1));
        list.step_filter(true);
        list.step_filter(true);
        list.push_search('O');
        list.push_search('v');
        assert_eq!(
            kept(&mut list),
            ("", "a1".to_owned(), 1),
            "a comment's text"
        );
        list.clear_search();
        "libb".chars().for_each(|c| list.push_search(c));
        assert_eq!(kept(&mut list), ("", "b1".to_owned(), 1), "a path, fuzzily");
        list.pop_search();
        list.pop_search();
        "zz".chars().for_each(|c| list.push_search(c));
        list.sync(&comments, false);
        assert!(
            matches!(list.rows(), [Row::Note(n)] if *n == [Part::Text("no thread matches /lizz".to_owned())])
        );
    }

    /// A selection stays on its thread while it's kept; a filter that drops
    /// it moves it to the next thread kept. Steps go over the headers and
    /// stop at either end.
    #[test]
    fn a_selection_keeps_its_thread_or_goes_to_the_next() {
        let dir = tempfile::tempdir().expect("tempdir");
        let comments = comments(&dir, THREADS, true);
        let mut list = List::new(&comments, false);
        let root = |list: &List, row| list.thread_at(row).map(|t| t.root.clone());
        let a1 = list.near((Some("a1"), b"")).expect("listed");
        assert_eq!(list.near((None, b"lib/b.rs")), list.row_of("b1"));
        let a3 = list.step(a1, 1);
        assert_eq!(root(&list, a3).as_deref(), Some("a3"));
        assert_eq!(list.step(a3, isize::MAX), a3, "stops at the end");
        assert_eq!(list.step(0, 1), 0, "a header isn't a thread's row");
        list.step_filter(false);
        assert!(list.sync(&comments, false));
        assert!(!list.sync(&comments, false), "made once");
        let kept = list.keep(Some("a3"), a3);
        assert_eq!(root(&list, kept).as_deref(), Some("a1"), "a3 is resolved");
        assert_eq!(list.row_of("a3"), None);
        let b1 = list.row_of("b1").expect("open");
        assert_eq!(list.keep(Some("b1"), 0), b1);
    }

    /// Outside a review session, threads on other branches fold into one
    /// row, pointing to what lists them; they're not counted with this
    /// branch's.
    #[test]
    fn other_branches_fold_into_a_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = THREADS.replace(
            r#""author":"bo","created":"2026-11-17T10:00:00Z","ref":"h:3""#,
            r#""author":"bo","created":"2026-11-17T10:00:00Z","branch":"topic""#,
        );
        let comments = comments(&dir, &json, false);
        let mut list = List::new(&comments, false);
        assert_eq!(roots(&list), ["a1", "a3"]);
        let folded = [
            Part::Text("1 thread on other branches".to_owned()),
            Part::Hint(Hint::OtherBranches),
        ];
        assert!(matches!(&list.rows()[0], Row::Note(n) if *n == folded));
        assert_eq!(list.listing().expect("made").counts, [2, 1]);
        list.toggle_others();
        list.sync(&comments, false);
        assert_eq!(roots(&list), ["a1", "a3", "b1"]);
    }
}
