//! What the control API's requests do to this view (`crate::control`),
//! and the events pushed to its subscribers. A request on a file whose diff
//! isn't loaded yet shows the file and waits for its diff, answered once
//! it's in. A request on a file the diff doesn't list waits before that
//! for the loop to find it, then opens it as the picker does
//! ([`App::other_found`]). With nobody subscribed and nothing waiting,
//! nothing here runs.

use std::time::{Duration, Instant};

use dffy_core::bstr::{BStr, BString};
use dffy_core::document::{File, Pos, Side, State};
use dffy_core::git::{Snapshot, Status};

use super::{Action, App, Pane, Seek};
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
    /// `None` while the loop looks up a file the diff doesn't list.
    file: Option<usize>,
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
    pub(super) fn control(&mut self, inbound: Inbound) -> Option<Action> {
        match inbound {
            Inbound::Command(command, reply) => return self.control_command(command, reply),
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
            Inbound::Listens(listens, reply) => self.listens(listens, reply),
            Inbound::Listened { request, facts } => self.listened(request, facts),
            // What waits for it goes as the event settles.
            Inbound::Cancelled => self.listeners_settled(),
            Inbound::Gone => {
                let c = &mut self.control;
                (c.push, c.selection, c.view, c.due) = (None, false, false, None);
                c.waiting.retain(|w| !w.reply.cancelled());
                self.listeners_gone();
            }
        }
        None
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

    fn control_command(&mut self, command: Command, reply: Reply) -> Option<Action> {
        // Its asker left while it waited behind the editor.
        if reply.cancelled() {
            return None;
        }
        // Moving the view under the text box, or the editor, would take
        // the comment being written somewhere it isn't about.
        if self.composing.is_some() && !matches!(command, Command::State | Command::Selection) {
            reply.send(Err(Fault::new(
                "failed",
                "a comment is being written in the viewer; ask again once it's saved",
            )));
            return None;
        }
        match command {
            Command::State => reply.send(Ok(self.control_state().encode())),
            Command::Selection => {
                reply.send(Ok(object(vec![("selection", self.control_selection())])));
            }
            Command::Open { path, line, side } => {
                return self.control_open(Waiting {
                    path: path.into(),
                    file: None,
                    side,
                    line,
                    comment: None,
                    reply,
                });
            }
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
                    return None;
                }
                return self.control_open(Waiting {
                    path: path.into(),
                    file: None,
                    side,
                    line: Some(line),
                    comment: Some((text, author)),
                    reply,
                });
            }
            Command::Focus { id } => reply.send(self.control_focus(&id)),
            Command::ApplySuggestion { id } => return self.control_apply(&id, reply),
        }
        None
    }

    /// Applies the suggestion of comment `id`, or of its thread's last
    /// comment with one, as `s` on its thread would. Refused while an ask
    /// runs: an agent asked to only read may reach this through dffy's own
    /// MCP server, which its harness can load, and applying writes files.
    fn control_apply(&mut self, id: &str, reply: Reply) -> Option<Action> {
        if self.asking.busy() {
            let key = (self.thread_key_name(crate::keys::Action::ApplySuggestion))
                .unwrap_or_else(|| "apply_suggestion".to_owned());
            reply.send(Err(Fault::new(
                "failed",
                format!("an agent ask is running; apply with {key} in the viewer"),
            )));
            return None;
        }
        let Some(comments) = &self.comments else {
            reply.send(Err(Fault::new(
                "failed",
                "this viewer has no comments file",
            )));
            return None;
        };
        let Some(root) = comments.thread_root(id) else {
            let missing = format!("no comment {id} in the comments this view shows");
            reply.send(Err(Fault::new("not_found", missing)));
            return None;
        };
        let path = comments.home_of(&root).unwrap_or_default();
        let at = (comments.threads(path.as_bytes()))
            .and_then(|threads| Some(threads.iter().find(|t| t.root == root)?.at.clone()));
        self.apply_suggestion((&root, at.as_ref()), Some(id), Some(reply))
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

    /// Shows `waiting`'s file and answers once its diff is in. One the
    /// diff doesn't list is for the loop to find first, at its side.
    fn control_open(&mut self, mut waiting: Waiting) -> Option<Action> {
        // A file opened outside the diff is looked up again, as the request
        // may ask for its other side.
        waiting.file = self.listed_entry(waiting.path.as_ref());
        let Some(file) = waiting.file else {
            // The view stays as it is until the file is found.
            return match self.unlisted_at(waiting.side) {
                Ok(at) => {
                    let path = waiting.path.clone();
                    let side = waiting.side;
                    self.control.waiting.push(waiting);
                    Some(Action::OpenOther { path, at, side })
                }
                Err(why) => {
                    let missing = format!("{} isn't in this diff; {why}", lossy(&waiting.path));
                    waiting.reply.send(Err(Fault::new("not_found", missing)));
                    None
                }
            };
        };
        // A line, even in the file shown, is a jump.
        if waiting.line.is_some() {
            self.leave();
        } else {
            self.leave_for(file);
        }
        if self.pane == Pane::Tree {
            self.leave_panel();
        }
        self.doc.selection = None;
        if self.doc.document.pos.file != file {
            self.doc.goto_file(file);
        }
        self.dirty = true;
        if let Some(waiting) = self.settle_waiting(waiting) {
            self.control.waiting.push(waiting);
        }
        None
    }

    /// Whether a request waiting for the loop to find `path` at `side`
    /// goes to a line in it.
    pub(super) fn control_wants_line(&self, path: &BStr, side: Side) -> bool {
        (self.control.waiting.iter())
            .any(|w| w.file.is_none() && w.side == side && w.path == path && w.line.is_some())
    }

    /// The snapshot to find a path the diff doesn't list at, for `side`;
    /// else why it isn't found.
    fn unlisted_at(&self, side: Side) -> Result<Snapshot, &'static str> {
        // A path the list hasn't reached yet would arrive as a second entry,
        // after one opened for it.
        if self.enumerating {
            return Err("still reading the diff's files");
        }
        self.snapshot(side)
    }

    /// Where the loop found `path`, which the diff didn't list, for the
    /// requests waiting on it at `side`: its entry, opened and shown;
    /// `Err(None)` when no file is there; `Err(Some(why))` when looking
    /// failed. Returns whether any were waiting, and so were told.
    pub(super) fn control_found(
        &mut self,
        path: &BStr,
        side: Side,
        found: Result<usize, Option<&str>>,
    ) -> bool {
        let (found_now, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.control.waiting)
            .into_iter()
            .partition(|w| w.file.is_none() && w.side == side && w.path == path);
        self.control.waiting = rest;
        let any = !found_now.is_empty();
        for mut waiting in found_now {
            let shown = lossy(&waiting.path);
            let fault = match found {
                Ok(file) => {
                    self.doc.selection = None;
                    waiting.file = Some(file);
                    if let Some(waiting) = self.settle_waiting(waiting) {
                        self.control.waiting.push(waiting);
                    }
                    continue;
                }
                Err(None) => Fault::new(
                    "not_found",
                    format!(
                        "{shown} isn't in this diff, nor at its {} side",
                        encode::side(side)
                    ),
                ),
                Err(Some(why)) => Fault::new("failed", format!("couldn't open {shown}: {why}")),
            };
            waiting.reply.send(Err(fault));
        }
        any
    }

    /// Answers `waiting` once its file's diff is loaded; hands it back
    /// while it's on its way.
    fn settle_waiting(&mut self, mut waiting: Waiting) -> Option<Waiting> {
        let Some(mut file) = waiting.file else {
            return Some(waiting);
        };
        let shown = lossy(&waiting.path);
        let files = self.doc.document.files();
        // The list may have been sorted or refreshed since.
        if files
            .get(file)
            .is_none_or(|e| e.change().path != waiting.path)
        {
            let Some(found) = self.control_file(&waiting.path) else {
                let gone = format!("{shown} isn't shown any more");
                waiting.reply.send(Err(Fault::new("not_found", gone)));
                return None;
            };
            file = found;
            waiting.file = Some(file);
        }
        // Its line would be read on the wrong version.
        if self.doc.document.files()[file].is_unchanged()
            && let Some(&at) = self.opened.get(waiting.path.as_slice())
            && at != waiting.side
        {
            let name = self.version_name(at);
            let moved = format!("{shown} was opened at {name} before it showed");
            waiting.reply.send(Err(Fault::new("failed", moved)));
            return None;
        }
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
            let diff = (self.doc.document.files()[file].file()).map(File::diff);
            let missing = if diff.is_some_and(|d| d.binary) {
                format!("{shown} is binary; no lines")
            } else if diff.is_some_and(|d| d.too_large) {
                format!("{shown} is too large to show; no lines")
            } else {
                format!("{shown} has no line {} on its {side} side here", line + 1)
            };
            waiting.reply.send(Err(Fault::new("not_found", missing)));
            return None;
        }
        self.dirty = true;
        self.kept = false;
        let listed = !self.doc.document.files()[file].is_unchanged();
        let added = match (waiting.comment, waiting.line) {
            (Some(comment), Some(line)) => self
                .control_comment(file, (waiting.side, line), comment)
                .map(Some),
            _ => Ok(None),
        };
        let result = added.map(|id| viewer::shown(id, shown, waiting.line, waiting.side, listed));
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
