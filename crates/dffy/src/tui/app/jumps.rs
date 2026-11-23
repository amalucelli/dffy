//! Jumping back and forward between the places explicit moves left: a
//! definition opened from a peek, or a file opened from find file, the
//! tree or the control API. A place is a file by its path, the diff's own
//! entry or one opened whole at a version, the snapshot its lines are
//! from, and the line at the top of the screen, so it outlives the list
//! being read again and entries opened or closed around it. Places are
//! taken on those moves only.

use std::collections::HashMap;

use dffy_core::bstr::{BStr, BString};
use dffy_core::document::{Entry, Pos, Side, State};
use dffy_core::git::Snapshot;

use super::{App, Pane};
use crate::render::escape;

/// The most places kept to go back to.
const KEPT: usize = 100;

#[derive(Default)]
pub(super) struct Jumps {
    back: Vec<Place>,
    forward: Vec<Place>,
    browsed: Option<Browsed>,
    /// A place gone to whose file's diff is still to come.
    landing: Option<Place>,
}

/// Where the tree took the keys, while its moves show the files passed.
struct Browsed {
    /// The place left; `None` on a commit or the conversation.
    place: Option<Place>,
    /// The entry shown as the tree took the keys, and the one it shows now.
    from: usize,
    at: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Place {
    path: BString,
    /// The version a file opened outside the diff shows; `None` for the
    /// diff's own entry.
    whole: Option<Side>,
    /// The line at the top of the screen, and its version.
    line: Option<(Side, usize)>,
    /// The snapshot of the place's version as it was taken: line numbers
    /// of another mean other lines.
    at: Option<Snapshot>,
    /// Screen rows from the top of the screen down to the line: the rows
    /// above it drawn then, as negative; rows of a wrapped line scrolled
    /// past, as positive.
    shift: isize,
}

impl Place {
    /// The version its file and line are read in.
    fn side(&self) -> Side {
        (self.whole)
            .or_else(|| self.line.map(|(side, _)| side))
            .unwrap_or(Side::New)
    }
}

impl Jumps {
    /// Whether there's a place to go back to, or `forward` to.
    pub(super) fn can_go(&self, forward: bool) -> bool {
        !if forward { &self.forward } else { &self.back }.is_empty()
    }

    /// A jump left `place`, `None` on a commit or the conversation: it goes
    /// on the back list, and the forward list and a landing waited on go.
    fn record(&mut self, place: Option<Place>) {
        self.forward.clear();
        self.landing = None;
        if let Some(place) = place {
            keep(&mut self.back, place);
        }
    }
}

fn keep(list: &mut Vec<Place>, place: Place) {
    if list.last() != Some(&place) {
        list.push(place);
    }
    if list.len() > KEPT {
        list.remove(0);
    }
}

/// The entries by path: the diff's own, by their path and a renamed one's
/// old path, the first of each kept, and those opened whole.
type ByPath<'a> = (HashMap<&'a BStr, usize>, HashMap<&'a BStr, usize>);

fn by_path<P>(files: &[Entry<P>]) -> ByPath<'_> {
    let (mut listed, mut whole) = (HashMap::new(), HashMap::new());
    for (i, e) in files.iter().enumerate() {
        if e.is_commit() {
            continue;
        }
        let path: &BStr = e.change().path.as_ref();
        if e.is_unchanged() {
            whole.entry(path).or_insert(i);
        } else {
            listed.entry(path).or_insert(i);
            listed.entry(e.change().old_path()).or_insert(i);
        }
    }
    (listed, whole)
}

impl App {
    /// Before a jump: the place it leaves, the tree's when its moves show
    /// another file, goes on the back list.
    pub(super) fn leave(&mut self) {
        let place = match self.take_browsed() {
            Some(browsed) => browsed.place,
            None => self.place(),
        };
        self.jumps.record(place);
    }

    /// Before showing entry `file`: a jump, unless it's the file shown, or
    /// after the tree's moves the one shown as the tree took the keys.
    pub(super) fn leave_for(&mut self, file: usize) {
        match self.take_browsed() {
            Some(browsed) if browsed.from != file => self.jumps.record(browsed.place),
            None if file != self.doc.document.pos.file => {
                let place = self.place();
                self.jumps.record(place);
            }
            Some(_) | None => {}
        }
    }

    /// The tree's moves while the diff still shows what they showed last.
    fn take_browsed(&mut self) -> Option<Browsed> {
        let shown = self.doc.document.pos.file;
        (self.jumps.browsed.take()).filter(|b| b.at == shown)
    }

    /// The tree's moves showed another file than the one shown as it took
    /// the keys: that was a jump from there.
    fn settle_browsed(&mut self) {
        if let Some(browsed) = self.take_browsed()
            && browsed.from != self.doc.document.pos.file
        {
            self.jumps.record(browsed.place);
        }
    }

    /// The tree shows `file` as its selection moves onto it. The first of
    /// these moves keeps the place it leaves.
    pub(super) fn browse(&mut self, file: usize) {
        let shown = self.doc.document.pos.file;
        if (self.jumps.browsed.as_ref()).is_none_or(|b| b.at != shown) {
            self.jumps.browsed = Some(Browsed {
                place: self.place(),
                from: shown,
                at: shown,
            });
        }
        self.doc.goto_file(file);
        if let Some(browsed) = &mut self.jumps.browsed {
            browsed.at = self.doc.document.pos.file;
        }
    }

    /// Back, or `forward`: the last place left that way whose file is
    /// still shown at its version, as it was at the top again. Moves in
    /// the tree that showed another file were a jump first.
    pub(super) fn jump(&mut self, forward: bool) {
        self.settle_browsed();
        let here = self.place();
        let mut gone = 0;
        let mut entries: Option<ByPath> = None;
        let found = loop {
            let list = if forward {
                &mut self.jumps.forward
            } else {
                &mut self.jumps.back
            };
            let Some(place) = list.pop() else {
                break None;
            };
            if Some(&place) == here.as_ref() {
                continue;
            }
            let (listed, whole) = entries.get_or_insert_with(|| by_path(self.doc.document.files()));
            let path: &BStr = place.path.as_ref();
            let file = match place.whole {
                None => listed.get(path),
                Some(_) => whole.get(path),
            };
            match file.copied().filter(|_| self.holds(&place)) {
                Some(file) => break Some((file, place)),
                None => gone += 1,
            }
        };
        let way = if forward { "forward" } else { "back" };
        let skipped = (gone > 0).then(|| format!("skipped {gone} no longer shown"));
        let Some((file, place)) = found else {
            self.message(match skipped {
                Some(skipped) => format!("nothing to go {way} to; {skipped}"),
                None => format!("nothing to go {way} to"),
            });
            return;
        };
        let (from, to) = if forward {
            (&mut self.jumps.forward, &mut self.jumps.back)
        } else {
            (&mut self.jumps.back, &mut self.jumps.forward)
        };
        let more = from.len();
        if let Some(here) = here {
            keep(to, here);
        }
        let mut words = format!("{way} to {}", self.place_name(&place));
        for part in skipped
            .into_iter()
            .chain((more > 0).then(|| format!("{more} more")))
        {
            words = format!("{words} · {part}");
        }
        self.message(words);
        self.doc.selection = None;
        self.doc.goto_file(file);
        self.leave_panel();
        self.jumps.landing = Some(place);
        self.jump_settle();
    }

    /// After each event: the tree's moves are settled once it loses the
    /// keys, and a place gone to shows as it was once its file's diff is
    /// in.
    pub(super) fn jump_settle(&mut self) {
        if self.pane != Pane::Tree && self.jumps.browsed.is_some() {
            self.settle_browsed();
        }
        let Some(place) = self.jumps.landing.take() else {
            return;
        };
        let file = self.doc.document.pos.file;
        if !self.shows(file, &place) {
            return;
        }
        match self.doc.document.files()[file].state() {
            State::Pending => self.jumps.landing = Some(place),
            State::Failed(_) => {}
            State::Loaded(_) => {
                if let Some((side, line)) = place.line {
                    if self.doc.line_to_top(file, side, line) {
                        self.doc.scroll(place.shift);
                    } else {
                        let version = self.version_name(side);
                        self.message(format!("no line {} in {version} here", line + 1));
                    }
                }
                self.dirty = true;
                self.kept = false;
            }
        }
    }

    /// Where the view is: the file shown, its version and the line at its
    /// top. `None` on a commit or the conversation, which have no path.
    fn place(&mut self) -> Option<Place> {
        let top = self.doc.document.pos;
        let file = top.file;
        let entry = self.doc.document.files().get(file)?;
        if entry.is_commit() {
            return None;
        }
        let path = entry.change().path.clone();
        let whole = (entry.is_unchanged()).then(|| self.opened_side(&path));
        let rows: Vec<(usize, Pos)> = (0..self.doc.height)
            .filter_map(|y| Some((y, self.doc.row_at(y)?)))
            .filter(|(_, pos)| pos.file == file)
            .collect();
        let first = (rows.into_iter())
            .map(|(y, pos)| (y, pos, self.doc.row_lines(pos)))
            .find(|(.., lines)| *lines != [None, None]);
        let (shift, [old, new]) = match first {
            Some((_, pos, lines)) if pos == (Pos { offset: 0, ..top }) => {
                (isize::try_from(top.offset).unwrap_or(0), lines)
            }
            Some((y, _, lines)) => (-isize::try_from(y).unwrap_or(0), lines),
            None => (0, [None, None]),
        };
        // A file opened whole has the same lines on both sides.
        let line = match whole {
            Some(side) => new.or(old).map(|line| (side, line)),
            None => {
                (new.map(|line| (Side::New, line))).or_else(|| old.map(|line| (Side::Old, line)))
            }
        };
        let mut place = Place {
            path,
            whole,
            line,
            at: None,
            shift,
        };
        place.at = self.snapshot(place.side()).ok();
        Some(place)
    }

    /// The version a file opened outside the diff shows; one opened for
    /// its comments shows the newer.
    fn opened_side(&self, path: &BString) -> Side {
        self.opened.get(path).copied().unwrap_or(Side::New)
    }

    /// Whether `place`'s version is still the one its file shows.
    fn holds(&self, place: &Place) -> bool {
        let opened = place
            .whole
            .is_none_or(|side| self.opened_side(&place.path) == side);
        opened && self.snapshot(place.side()).ok() == place.at
    }

    /// Whether entry `file` shows `place`'s file at its version.
    fn shows(&self, file: usize, place: &Place) -> bool {
        let Some(e) = self.doc.document.files().get(file) else {
            return false;
        };
        let path: &BStr = place.path.as_ref();
        let same = match place.whole {
            Some(_) => e.is_unchanged() && e.change().path == path,
            None => {
                !e.is_commit()
                    && !e.is_unchanged()
                    && (e.change().path == path || e.change().old_path() == path)
            }
        };
        same && self.holds(place)
    }

    /// `fs/namei.c:120`, and the version when it's the older one.
    fn place_name(&self, place: &Place) -> String {
        let path = escape(&place.path);
        match place.line {
            Some((Side::Old, line)) => {
                format!("{path}:{} in {}", line + 1, self.version_name(Side::Old))
            }
            Some((Side::New, line)) => format!("{path}:{}", line + 1),
            None => path,
        }
    }
}
