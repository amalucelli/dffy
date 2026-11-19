//! What the control API's requests do to this view (`crate::control`),
//! and the events pushed to its subscribers. A request on a file whose diff
//! isn't loaded yet shows the file and waits for its diff, answered once
//! it's in. With nobody subscribed and nothing waiting, nothing here runs.

use std::time::{Duration, Instant};

use dffy_core::bstr::BString;
use dffy_core::document::{Pos, Side, State};
use dffy_core::git::Status;

use super::{App, Pane, Seek};
use crate::control::viewer::{self, Command, Inbound, Push, Reply};
use crate::control::{Fault, object};
use crate::encode;
use crate::plugin::json::Value;
use crate::review::Op;
use crate::review::model;

/// How long the view stays put before what changed is pushed.
const DEBOUNCE: Duration = Duration::from_millis(50);

#[derive(Default)]
pub(super) struct Control {
    /// Requests waiting for their file's diff.
    waiting: Vec<Waiting>,
    /// Set while someone subscribes to one of the view's events.
    push: Option<Push>,
    selection: bool,
    view: bool,
    /// The selection and the position when last looked at.
    seen: Option<Looks>,
    /// When to push what changed, once nothing more did.
    due: Option<Instant>,
    /// The selection and the state last pushed.
    sent: [Option<Value>; 2],
    /// The selected lines the selection pushed was copied from.
    lines: Option<(usize, Side, usize, usize)>,
}

type Looks = (Option<(usize, Side, usize, usize)>, Pos);

/// A `view/open` or `comments/add` waiting for its file's diff.
struct Waiting {
    path: BString,
    file: usize,
    side: Side,
    line: Option<usize>,
    /// The text and the author of the comment to add on `line`, for a
    /// `comments/add`.
    comment: Option<(String, String)>,
    reply: Reply,
}

impl Control {
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.due
    }
}

fn lossy(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

impl App {
    pub(super) fn control(&mut self, inbound: Inbound) {
        match inbound {
            Inbound::Command(command, reply) => self.control_command(command, reply),
            Inbound::Wanted {
                selection,
                view,
                push,
            } => {
                // An event newly wanted goes out with what's there now, so a
                // change while the subscription was on its way isn't lost;
                // dffyd gives later subscribers the last push.
                let c = &mut self.control;
                for (i, wanted) in [selection, view].into_iter().enumerate() {
                    if !wanted || ![c.selection, c.view][i] {
                        c.sent[i] = None;
                    }
                }
                c.selection = selection;
                c.view = view;
                c.push = (selection || view).then_some(push);
                c.seen = None;
                c.due = None;
            }
            Inbound::Unreachable(why) => {
                self.message(format!("dffy ctl can't reach this viewer: {why}"));
            }
            // What waits for it goes as the event settles.
            Inbound::Cancelled => {}
        }
    }

    /// After each event: requests whose diff came are answered, unless a
    /// comment is being written, which they'd move the view from, and once
    /// the selection or the position stayed put for [`DEBOUNCE`] after a
    /// change, the events subscribed to are pushed. Requests whose asker
    /// left go.
    pub(super) fn control_settle(&mut self) {
        if self.control.waiting.is_empty() && self.control.push.is_none() {
            return;
        }
        self.control.waiting.retain(|w| !w.reply.cancelled());
        if self.composing.is_none() {
            for waiting in std::mem::take(&mut self.control.waiting) {
                if let Some(waiting) = self.settle_waiting(waiting) {
                    self.control.waiting.push(waiting);
                }
            }
        }
        if self.control.push.is_none() {
            return;
        }
        let looks = self.control_looks();
        if self.control.seen.as_ref() != Some(&looks) {
            self.control.seen = Some(looks);
            self.control.due = Some(Instant::now() + DEBOUNCE);
        } else if self.control.due.is_some_and(|at| Instant::now() >= at) {
            self.control.due = None;
            self.control_push();
        }
    }

    fn control_looks(&self) -> Looks {
        (self.doc.selected_lines(), self.doc.document.pos)
    }

    fn control_push(&mut self) {
        let Some(push) = self.control.push.clone() else {
            return;
        };
        // The selection is copied again only when its lines moved, not on
        // every move of the view.
        let lines = self.doc.selected_lines();
        if self.control.selection && (self.control.sent[0].is_none() || self.control.lines != lines)
        {
            self.control.lines = lines;
            let selection = self.control_selection();
            if self.control.sent[0].as_ref() != Some(&selection) {
                push.send(
                    "selection/changed",
                    object(vec![("selection", selection.clone())]),
                );
                self.control.sent[0] = Some(selection);
            }
        }
        if self.control.view {
            let state = self.control_state().encode();
            if self.control.sent[1].as_ref() != Some(&state) {
                push.send("view/changed", state.clone());
                self.control.sent[1] = Some(state);
            }
        }
    }

    fn control_command(&mut self, command: Command, reply: Reply) {
        // Its asker left while it waited behind the editor.
        if reply.cancelled() {
            return;
        }
        // Moving the view under the text box, or the editor, would take
        // the comment being written somewhere it isn't about.
        if self.composing.is_some() && !matches!(command, Command::State | Command::Selection) {
            reply.send(Err(Fault::new(
                "failed",
                "a comment is being written in the viewer; ask again once it's saved",
            )));
            return;
        }
        match command {
            Command::State => reply.send(Ok(self.control_state().encode())),
            Command::Selection => {
                reply.send(Ok(object(vec![("selection", self.control_selection())])));
            }
            Command::Open { path, line, side } => self.control_open(path, line, side, None, reply),
            Command::Comment {
                path,
                line,
                side,
                text,
                author,
            } => {
                if self.comments.is_none() {
                    reply.send(Err(Fault::new(
                        "failed",
                        "this viewer has no comments file: start it with --comments <file>",
                    )));
                    return;
                }
                self.control_open(path, Some(line), side, Some((text, author)), reply);
            }
            Command::Focus { id } => reply.send(self.control_focus(&id)),
        }
    }

    /// The entry of the file at `path`, by its path or, renamed, its old
    /// one.
    fn control_file(&self, path: &[u8]) -> Option<usize> {
        self.doc.document.files().iter().position(|e| {
            let change = e.change();
            !e.is_commit()
                && (change.path == path
                    || matches!(&change.status, Status::Renamed { from, .. } if from == path))
        })
    }

    fn control_open(
        &mut self,
        path: String,
        line: Option<usize>,
        side: Side,
        comment: Option<(String, String)>,
        reply: Reply,
    ) {
        let Some(file) = self.control_file(path.as_bytes()) else {
            let listing = if self.enumerating {
                "; files are still being listed"
            } else {
                ""
            };
            reply.send(Err(Fault::new(
                "not_found",
                format!("{path} isn't in this diff{listing}"),
            )));
            return;
        };
        if self.pane == Pane::Tree {
            self.leave_panel();
        }
        self.doc.selection = None;
        if self.doc.document.pos.file != file {
            self.doc.goto_file(file);
        }
        self.dirty = true;
        let waiting = Waiting {
            path: path.into(),
            file,
            side,
            line,
            comment,
            reply,
        };
        if let Some(waiting) = self.settle_waiting(waiting) {
            self.control.waiting.push(waiting);
        }
    }

    /// Answers `waiting` once its file's diff is loaded; hands it back
    /// while it's on its way.
    fn settle_waiting(&mut self, mut waiting: Waiting) -> Option<Waiting> {
        let shown = lossy(&waiting.path);
        let files = self.doc.document.files();
        // The list may have been sorted or refreshed since.
        if files
            .get(waiting.file)
            .is_none_or(|e| e.change().path != waiting.path)
        {
            let Some(file) = self.control_file(&waiting.path) else {
                let gone = format!("{shown} left the diff");
                waiting.reply.send(Err(Fault::new("not_found", gone)));
                return None;
            };
            waiting.file = file;
        }
        let file = waiting.file;
        match self.doc.document.files()[file].state() {
            State::Loaded(_) => {}
            State::Failed(why) => {
                let why = format!("{shown} couldn't be diffed: {why}");
                waiting.reply.send(Err(Fault::new("failed", why)));
                return None;
            }
            State::Pending if self.doc.document.pos.file == file => return Some(waiting),
            State::Pending => {
                let moved = format!("the view went to another file before {shown}'s diff came");
                waiting.reply.send(Err(Fault::new("failed", moved)));
                return None;
            }
        }
        let side = encode::side(waiting.side);
        if let Some(line) = waiting.line
            && !self.doc.goto_line(file, waiting.side, line)
        {
            let missing = format!("{shown} has no line {} on its {side} side here", line + 1);
            waiting.reply.send(Err(Fault::new("not_found", missing)));
            return None;
        }
        self.dirty = true;
        self.kept = false;
        let added = match (waiting.comment, waiting.line) {
            (Some(comment), Some(line)) => self
                .control_comment(file, (waiting.side, line), comment)
                .map(Some),
            _ => Ok(None),
        };
        let result = added.map(|id| viewer::shown(id, shown, waiting.line, waiting.side));
        waiting.reply.send(result);
        None
    }

    /// Adds `comment` on line `line` of `side` of `file`, a loaded file, as
    /// `c` would, and focuses its thread; returns its id. It's the control
    /// client's, by its name, not yours: in a review session it's posted
    /// only once `a` makes it yours.
    fn control_comment(
        &mut self,
        file: usize,
        (side, line): (Side, usize),
        (text, author): (String, String),
    ) -> Result<String, Fault> {
        let entry = &self.doc.document.files()[file];
        let failed = |why: &str| Fault::new("failed", why);
        let loaded = entry
            .file()
            .ok_or_else(|| failed("its diff isn't loaded"))?;
        let mut draft = model::compose::draft(
            entry.change(),
            model::line_diff(loaded),
            side,
            line..line + 1,
        )
        .map_err(failed)?;
        draft.text = text;
        draft.author = author;
        let op = Op::Add {
            comment: Box::new(self.on_branch(draft)),
            reply_to: None,
        };
        let ids = self
            .write_comments(vec![op], "comment added through dffy ctl")
            .map_err(|e| failed(&e))?;
        let id = (ids.into_iter().next().flatten()).ok_or_else(|| failed("no id came back"))?;
        self.sync_threads();
        if !self.doc.focus_thread(&id) {
            self.thread_pending = Some((file, Seek::Saved(id.clone())));
        }
        Ok(id)
    }

    /// Shows and focuses the thread comment `id` is in, as the comments
    /// panel does; one not placed yet is focused once it is.
    fn control_focus(&mut self, id: &str) -> Result<Value, Fault> {
        let not_found = |why: String| Fault::new("not_found", why);
        let comments = (self.comments.as_ref())
            .ok_or_else(|| Fault::new("failed", "this viewer has no comments file"))?;
        let root = (comments.thread_root(id))
            .ok_or_else(|| not_found(format!("no comment {id} in the comments this view shows")))?;
        if comments.elsewhere(&root) {
            return Err(not_found(format!(
                "thread {root} is on another branch, so the diff doesn't show it"
            )));
        }
        let path = comments.home_of(&root).unwrap_or_default().to_owned();
        // A general comment is on the conversation page, the first entry.
        let file = if path.is_empty() {
            (self.doc.document.files().first())
                .is_some_and(super::view::Entry::is_page)
                .then_some(0)
        } else {
            comments.file_of(path.as_bytes())
        };
        let file = file.ok_or_else(|| {
            not_found(format!(
                "{path} isn't in this diff, so thread {root} isn't shown"
            ))
        })?;
        if self.pane == Pane::Tree {
            self.leave_panel();
        }
        self.doc.selection = None;
        self.doc.goto_file(file);
        self.sync_threads();
        let focused = self.doc.go_to_thread(&root);
        if !focused {
            self.doc.set_focus(None);
            self.thread_pending = Some((file, Seek::Root(root.clone())));
        }
        self.dirty = true;
        Ok(viewer::focused(root, path, focused))
    }

    fn control_state(&mut self) -> viewer::State {
        let files = self.doc.document.files();
        let entry = (files.get(self.doc.document.pos.file)).filter(|e| !e.is_commit());
        let old_path = match entry.map(|e| &e.change().status) {
            Some(Status::Renamed { from, .. }) => Some(lossy(from)),
            _ => None,
        };
        viewer::State {
            path: entry.map(|e| lossy(&e.change().path)),
            old_path,
            files: files.len(),
            top: self.doc.top_lines(),
            listing: self.enumerating,
            split: self.doc.layout.split,
            diff: self.description.clone(),
            revisions: (self.selected.as_ref())
                .map(|(r, _)| r.revs.clone())
                .unwrap_or_default(),
        }
    }

    fn control_selection(&mut self) -> Value {
        let selection = self
            .doc
            .selected_lines()
            .and_then(|(file, side, start, end)| {
                let path = lossy(&self.doc.document.files().get(file)?.change().path);
                let copied = self.doc.selected(false);
                Some(viewer::Selection {
                    path,
                    side,
                    start,
                    end,
                    text: copied.map_or_else(String::new, |c| lossy(&c.text)),
                })
            });
        viewer::selection(selection.as_ref())
    }
}
