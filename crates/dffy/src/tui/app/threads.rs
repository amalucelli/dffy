//! Comment threads in the viewer: the comments file shown, the threads
//! placed on each file's diff, writing comments in the text box or the
//! editor, the thread's keys, and the comments panel.

use std::fmt::Write as _;
use std::ops::{ControlFlow, Range};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use dffy_core::annotation::{self, Anchor, AnnotationSet};
use dffy_core::bstr::BString;
use dffy_core::document::{Side, Source, State};

use super::{
    Action, App, Applying, Checkout, Confirmed, Handed, Handoff, OnSave, POSTING, Pane,
    STILL_EDITING, Seek, Suggestion, WHEEL_ROWS, Watch,
};
use crate::control::Fault;
use crate::control::viewer::Reply;
use crate::keys::Action as Act;
use crate::render::escape;
use crate::review::model::compose::Compose;
use crate::review::model::{self, Comments};
use crate::review::{Change, Op, Target, prompt, session};
use crate::term::{Input, Key, Mods};
use crate::tui::comments::{self, Panel, Writes, Writing};
use crate::tui::textbox::TextBox;
use crate::tui::tree::Item;
use crate::tui::view;
use crate::tui::view::Stepped;

impl App {
    /// Takes the comments in `path`, reloaded whenever [`Event::Comments`]
    /// says it changed.
    pub(crate) fn set_comments(&mut self, path: PathBuf) {
        let session = self.reviewing.mode.is_some();
        let mut comments = Comments::new(
            path,
            self.local_comments.clone(),
            session,
            self.branch.clone(),
        );
        // On the review plain dffy found, what's written is the working
        // tree's until it's posted.
        comments.set_new_local(
            self.reviewing
                .mode
                .as_ref()
                .is_some_and(|m| m.plain.is_some()),
        );
        comments.set_highlight(self.highlight);
        self.comments = Some(comments);
        self.reload_comments(model::reload::Check::Stat);
        // A file with no comments reads as unchanged, yet replaces one that
        // may have had a conversation.
        self.sync_conversation();
    }

    /// Takes the local review's file, whose drafts show on a review
    /// session's diff.
    pub(crate) fn set_local_comments(&mut self, path: Option<PathBuf>) {
        self.local_comments = path;
    }

    /// Takes the current branch, before the comments are read: the local
    /// review's threads on other branches stay out of view.
    pub(crate) fn set_branch(&mut self, branch: Option<String>) {
        self.branch = branch;
    }

    /// Takes HEAD's commit, read after the first frame.
    pub(crate) fn set_head(&mut self, head: Option<String>) {
        self.head = head;
    }

    /// HEAD is on `branch` now: the local review's threads shown are its,
    /// and new ones go on it.
    pub(super) fn branch_moved(&mut self, branch: Option<String>) {
        if branch == self.branch {
            return;
        }
        self.branch = branch;
        let path = (self.comments.as_ref()).and_then(|c| c.sources().into_iter().next());
        if let Some(path) = path {
            self.set_comments(path);
        }
    }

    /// The files [`App::set_comments`] with `path` reads, to watch.
    pub(crate) fn comment_files(&self, path: PathBuf) -> Vec<PathBuf> {
        Comments::new(path, self.local_comments.clone(), false, None).sources()
    }

    /// Reads the comments file again, if it changed since the last read,
    /// as the review session says (see [`Reviewing::reload`]).
    pub(super) fn reload_comments(&mut self, check: model::reload::Check) {
        let Some(comments) = &mut self.comments else {
            return;
        };
        let effects = self.reviewing.reload(comments, check, Instant::now());
        self.review_effects(effects);
    }

    /// After `changed` paths' comments changed: finds them in the list.
    /// Their threads are made again at once; comments on lines new since
    /// they were placed show once the pool places them. A file with no
    /// lines to find, as one whose diff failed, is placed here.
    pub(super) fn placed_again(&mut self, changed: &[BString]) {
        self.sync_conversation();
        let Some(comments) = &mut self.comments else {
            return;
        };
        self.annotations_changed = true;
        let files = self.doc.document.files();
        comments.locate(files);
        if self.tree.commented() {
            self.tree.set_commented(Some(comments.paths()));
        }
        for path in changed {
            let Some(entry) = comments.file_of(path).and_then(|f| files.get(f)) else {
                continue;
            };
            let lines = comments
                .annotated(path)
                .is_some_and(|a| !a.annotations.is_empty());
            let at_once = match entry.state() {
                State::Failed(_) => true,
                State::Loaded(_) => !lines,
                State::Pending => false,
            };
            if at_once {
                comments.place_without_diff(path, entry.version());
            }
        }
        self.dirty = true;
    }

    /// Puts the conversation page first while a review session's comments
    /// have general comments, sized to them, and takes it away when they
    /// have none. The page's threads are made once it shows. While no file
    /// is listed it waits, so the viewer still opens on the first file.
    pub(super) fn sync_conversation(&mut self) {
        let count = self
            .comments
            .as_ref()
            .filter(|_| self.reviewing.mode.is_some())
            .map_or(0, Comments::conversation_len);
        let files = self.doc.document.files();
        let shown = files.first().map(view::Entry::source);
        let shown = match shown {
            Some(Source::Page { rows }) if *rows == count + 1 => return,
            Some(Source::Page { .. }) => true,
            _ => false,
        };
        if !shown && (count == 0 || files.is_empty()) {
            return;
        }
        if let Some(mode) = &self.reviewing.mode {
            let title = mode.session.title.as_deref().unwrap_or(&mode.label);
            title.clone_into(&mut self.doc.page_title);
        }
        let moved = self.doc.set_page((count > 0).then_some(count));
        if moved != 0 {
            self.files_moved(moved);
        }
        self.dirty = true;
        self.kept = false;
    }

    /// After every entry moved by `by`, the page having come or gone: what
    /// holds a file by its place in the list follows it.
    fn files_moved(&mut self, by: isize) {
        let shift = |file: &mut usize| *file = file.saturating_add_signed(by);
        if let Some(search) = &mut self.search {
            if by > 0 {
                search.counts.insert(0, None);
            } else if !search.counts.is_empty() {
                search.counts.remove(0);
            }
            if let Some((file, _)) = &mut search.pending {
                shift(file);
            }
        }
        if let Some(target) = &mut self.render {
            shift(&mut target.file);
        }
        if let Some((file, _)) = &mut self.thread_pending {
            shift(file);
        }
        let visible = &mut self.visible;
        *visible = visible.start.saturating_add_signed(by)..visible.end.saturating_add_signed(by);
        // Positions in the old list, a click's or a tap's, are dropped.
        self.tapped = None;
        self.press = None;
        self.rebuild_tree();
        if let Some(picker) = &mut self.picker {
            picker.rebuild(self.doc.document.files());
        }
        self.sync_threads();
    }

    /// Takes where the comments of `file`, just diffed or failed to be,
    /// landed on it, as `placed` says, and reveals the lines of each,
    /// folded or not, that a gap hid, so it hangs under its own line;
    /// positions stay on their lines. Placements made before the comments
    /// changed are dropped: the file's are asked for again.
    pub(super) fn placed(&mut self, file: usize, placed: Option<model::Landed>) {
        let (Some(comments), Some(entry)) =
            (&mut self.comments, self.doc.document.files().get(file))
        else {
            return;
        };
        let (path, version) = (entry.change().path.clone(), entry.version());
        // Nothing to find lines for: no diff, or only comments on the file.
        let without_diff = match (entry.state(), &placed) {
            (State::Failed(_), _) => true,
            (State::Loaded(_), None) => comments
                .annotated(&path)
                .is_some_and(|a| a.annotations.is_empty()),
            _ => false,
        };
        let revealed = match (entry.state(), placed) {
            _ if without_diff => {
                comments.place_without_diff(&path, version);
                false
            }
            (State::Loaded(_), Some(landed)) => {
                let Some((placements, drafts_moved)) = comments.place(&path, version, landed)
                else {
                    return;
                };
                let revealed = self.doc.reveal_annotations(file, placements);
                if drafts_moved {
                    self.plan_soon();
                }
                revealed
            }
            _ => return,
        };
        if revealed {
            self.doc.normalize();
            self.dirty = true;
            self.kept = false;
        }
        // Its count, made without these threads, is made again; the file
        // shown is searched again with its blocks.
        let marked = (self.doc.document.search.as_ref()).is_some_and(|m| m.file == file);
        if let Some(count) = (self.search.as_mut()).and_then(|s| s.counts.get_mut(file))
            && !marked
        {
            *count = None;
        }
        // `m`, `M` or the panel went here before its threads were.
        if self.doc.document.pos.file == file
            && let Some((_, seek)) = self.thread_pending.take_if(|(pending, _)| *pending == file)
        {
            self.sync_threads();
            match seek {
                Seek::Edge(last) => self.doc.edge_thread(last),
                Seek::Root(root) => self.doc.go_to_thread(&root),
                Seek::Saved(root) => self.doc.focus_thread(&root),
            };
        }
    }

    /// Writes `ops` to the comments file and says `note`, or why not.
    /// Returns the id each operation touched, or why nothing was written.
    /// Nothing is while a post runs, unless the viewer is stopping: the
    /// plugin is writing what it posted to the file, and a draft changed
    /// meanwhile would go up as it was, or not be found.
    pub(super) fn write_comments(
        &mut self,
        ops: Vec<Op>,
        note: &str,
    ) -> Result<Vec<Option<String>>, String> {
        if self.writes_held() {
            self.message(POSTING);
            return Err(POSTING.to_owned());
        }
        let Some(comments) = self.comments.as_mut() else {
            return Err("no comments file".to_owned());
        };
        match comments.apply(ops) {
            Ok((written, ids)) => {
                self.plan_soon();
                self.placed_again(&written.changed);
                match written.unread {
                    Some(e) => self.message(format!("{note} · but comments: {e}")),
                    None => self.message(note),
                }
                Ok(ids)
            }
            Err(e) => {
                self.message(format!("comments: {e}"));
                Err(e)
            }
        }
    }

    /// Whether writes to the comments wait for a post to end.
    pub(super) fn writes_held(&self) -> bool {
        self.posting_now() && !self.leaving
    }

    /// The editor closed with `text` written, or `None` when it failed:
    /// saves what was being written, unless it's empty or unchanged. Text
    /// the box handed over opens in the box again when the editor failed.
    /// When the save fails, the text opens in the box to try again where
    /// the diff shows its place; else it's returned, for the caller to
    /// keep.
    pub(crate) fn composed(&mut self, text: Option<String>) -> Option<String> {
        let handed = self.handed.take();
        let compose = self.composing.take()?;
        let Some(text) = text else {
            if let Some((text, at)) = handed {
                let after = self.on_save;
                self.open_box(compose, at, &text);
                self.on_save = after;
            }
            return None;
        };
        let at = handed.map(|(_, at)| at).or_else(|| self.box_for(&compose));
        let reopens = at.is_some();
        (!self.save(compose, at, &text, By::Editor) && !reopens).then_some(text)
    }

    /// Where the text box for `compose` goes, when the diff shows it: a new
    /// comment's file shown, or the thread replied in or edited.
    fn box_for(&self, compose: &Compose) -> Option<Writes> {
        let shown = |root: &str| self.doc.threads().iter().any(|t| t.root == root);
        match compose {
            Compose::New(comment) => {
                let entry = self.doc.document.files().get(self.doc.document.pos.file)?;
                if entry.change().path != comment.path.as_bytes() {
                    return None;
                }
                Some(match comment.range() {
                    Some((side, lines)) => Writes::New {
                        side: match side {
                            crate::review::Side::Old => Side::Old,
                            crate::review::Side::New => Side::New,
                        },
                        lines,
                    },
                    None => Writes::File,
                })
            }
            Compose::Reply(root) => shown(root).then(|| Writes::Reply(root.clone())),
            Compose::Edit { id, .. } => {
                let root = self.comments.as_ref()?.thread_root(id)?;
                shown(&root).then(|| Writes::Edit {
                    root,
                    id: id.clone(),
                })
            }
        }
    }

    /// Saves `text` as `compose` says, and focuses its thread, but for a
    /// new one sent with `enter` that asks nothing: the keys stay the
    /// diff's, and `m` goes to it next. When the write fails, the text
    /// stays in the box (`at`) to try again. Returns whether it was saved,
    /// or there was nothing to save.
    fn save(&mut self, compose: Compose, at: Option<Writes>, text: &str, by: By) -> bool {
        let root = match &compose {
            Compose::New(_) => None,
            Compose::Reply(root) => Some(root.clone()),
            Compose::Edit { id, .. } => self.comments.as_ref().and_then(|c| c.thread_root(id)),
        };
        let new = matches!(compose, Compose::New(_));
        let edit = matches!(compose, Compose::Edit { .. });
        // Only a comment you write here asks: one that comes any other way
        // never runs an agent.
        let after = std::mem::take(&mut self.on_save);
        let mention = (!edit).then(|| self.asking.mentioned(text)).flatten();
        if self.writes_held() {
            // Kept as typed, to save once the post is done.
            if let Some(at) = at {
                self.open_box(compose, at, text);
                self.on_save = after;
            }
            self.message(POSTING);
            return false;
        }
        let retry = at.map(|at| (compose.clone(), at));
        // Kept local like a mention: posted, a question or a note to an
        // agent is out of context.
        let to_agent = matches!(after, OnSave::Ask | OnSave::Keep | OnSave::Fix);
        let Some((mut op, note)) = compose.into_op(text) else {
            match (&root, after) {
                // Sent empty, a question's reply asks about the thread as it is.
                (Some(root), OnSave::Ask) if !edit => {
                    let answered = self.answered_by(root);
                    self.ask_about((root, None), (answered, None));
                }
                _ if edit => self.message("nothing changed; nothing saved"),
                _ => self.message("nothing written; nothing saved"),
            }
            return true;
        };
        if let Op::Add { comment, .. } = &mut op
            && (mention.is_some() || to_agent)
        {
            comment.local_only = true;
        }
        // A follow-up goes to the agent whose answer it follows.
        let answered = (after == OnSave::Ask)
            .then(|| root.as_deref().and_then(|r| self.answered_by(r)))
            .flatten();
        // An ask keeps its thread focused, where `esc` stops it.
        let asks = match (&mention, after) {
            (_, OnSave::Keep) | (None, OnSave::Nothing | OnSave::Fix) => false,
            (Some(_), _) | (None, OnSave::Ask) => true,
        };
        let focus = !(new && by == By::Enter && !asks);
        let note = match (new, by) {
            (true, By::Enter) if !asks => match self.key_for(false, Act::NextThread) {
                Some(key) => format!("{note} · {key} goes to it"),
                None => note.to_owned(),
            },
            (true, By::Esc) => match self.thread_key_name(Act::DeleteDraft) {
                Some(key) => format!("draft kept · {key} {key} deletes"),
                None => "draft kept".to_owned(),
            },
            _ => note.to_owned(),
        };
        if let Ok(ids) = self.write_comments(vec![op], &note) {
            let written = ids.into_iter().next().flatten();
            let root = root.or_else(|| written.clone());
            self.sync_threads();
            if let Some(root) = root.as_deref() {
                match (mention, after) {
                    (_, OnSave::Keep) | (None, OnSave::Nothing) => {}
                    (Some(agent), _) => self.ask_about((root, None), (Some(agent), written)),
                    (None, OnSave::Ask) => self.ask_about((root, None), (answered, None)),
                    (None, OnSave::Fix) => self.fix(root),
                }
            }
            match root {
                Some(root) if !focus => self.just_saved = Some((root, self.inputs)),
                // A thread on new lines shows once the pool placed it.
                Some(root) if !self.doc.focus_thread(&root) => {
                    self.thread_pending = Some((self.doc.document.pos.file, Seek::Saved(root)));
                }
                _ => {}
            }
            return true;
        }
        if let Some((compose, at)) = retry {
            self.open_box(compose, at, text);
            self.on_save = after;
            self.save_failed = true;
            if let Some((message, _)) = &mut self.message {
                *message = format!("not saved, esc again discards · {message}");
            }
        }
        false
    }

    fn open_box(&mut self, compose: Compose, at: Writes, text: &str) {
        self.on_save = OnSave::Nothing;
        self.composing = Some(compose);
        self.doc.selection = None;
        self.doc.write(Writing {
            at,
            text: TextBox::new(text),
        });
        self.reveal_box();
    }

    /// Scrolls the text box into view, through its cursor.
    fn reveal_box(&mut self) {
        if let Some(block) = self.doc.writing_block() {
            self.doc.reveal_block(block, true);
        }
    }

    /// Input while the text box is open: every key edits it but `enter`,
    /// which sends it (saves it, and asks when it's for an agent; empty, a
    /// question's reply asks about the thread), `esc`, which saves it
    /// without asking (an empty text discards it, and so does a second
    /// `esc` after a save failed), `ctrl+g`, which hands it to the editor,
    /// and `ctrl+c`, which saves it and quits. The wheel still scrolls.
    pub(super) fn writing_input(&mut self, input: Input) -> Option<Action> {
        let ctrl = |mods: Mods| mods.contains(Mods::CTRL);
        let failed = std::mem::take(&mut self.save_failed);
        match input {
            Input::Key(Key::Esc, _) if failed => {
                self.doc.stop_writing();
                self.composing = None;
                self.on_save = OnSave::Nothing;
                self.message("discarded");
            }
            Input::Key(Key::Esc, _) => self.close_box(),
            Input::Key(Key::Enter, Mods::NONE) => self.send_box(By::Enter),
            Input::Key(Key::Char('c'), mods) if ctrl(mods) => {
                self.close_box();
                return Some(Action::Quit);
            }
            // Not ctrl+e: Ghostty sends it for cmd+right, which the box
            // takes as the end of the row.
            Input::Key(Key::Char('g'), mods) if ctrl(mods) => {
                let writing = self.doc.stop_writing()?;
                let text = writing.text.text().to_owned();
                self.handed = Some((text.clone(), writing.at));
                return Some(Action::Compose(text));
            }
            Input::Key(key, mods) => {
                self.doc.edit_writing(|w| w.text.key(key, mods));
                self.reveal_box();
            }
            Input::Paste(text) => {
                self.doc.edit_writing(|w| w.text.paste(&text));
                self.reveal_box();
            }
            Input::Scroll { delta, .. } => self.scroll_diff(isize::from(delta) * WHEEL_ROWS),
            _ => {}
        }
        None
    }

    /// Closes the text box, saving its text. Only `enter` sends a question
    /// ([`App::send_box`]): closed any other way, it's kept, local only, and
    /// asks nothing.
    pub(super) fn close_box(&mut self) {
        if self.on_save == OnSave::Ask {
            self.on_save = OnSave::Keep;
        }
        self.send_box(By::Esc);
    }

    /// Closes the text box, saving its text and doing what saving it does.
    fn send_box(&mut self, by: By) {
        let Some(writing) = self.doc.stop_writing() else {
            return;
        };
        let Some(compose) = self.composing.take() else {
            return;
        };
        let text = writing.text.text().to_owned();
        // An empty question's reply goes on to ask about the thread.
        let asks = matches!(compose, Compose::Reply(_)) && self.on_save == OnSave::Ask;
        if text.trim().is_empty() && !matches!(compose, Compose::Edit { .. }) && !asks {
            self.on_save = OnSave::Nothing;
            self.message("empty; nothing saved");
            return;
        }
        self.save(compose, Some(writing.at), &text, by);
    }

    /// A reply or an edit whose thread left the file shown, as when an
    /// import removed it, keeps its text: the box writes a comment on the
    /// file from then on, or where there's no file to comment on, the text
    /// goes to a file of its own ([`App::take_orphaned`]). Either is said.
    pub(super) fn keep_orphaned_box(&mut self) {
        // Gone from the file, not only from the threads shown, as a thread
        // waiting to be placed again after its diff changed is.
        let gone = self.doc.writing().is_some_and(|w| match &w.at {
            Writes::Reply(root) | Writes::Edit { root, .. } => {
                !self.doc.threads().iter().any(|t| t.root == *root)
                    && (self.comments.as_ref()).is_none_or(|c| c.thread_root(root).is_none())
            }
            Writes::New { .. } | Writes::File => false,
        });
        if !gone {
            return;
        }
        let Some(writing) = self.doc.stop_writing() else {
            return;
        };
        self.composing = None;
        let text = writing.text.text().to_owned();
        let entry = self.doc.document.files().get(self.doc.document.pos.file);
        let draft = entry
            .filter(|e| !e.is_commit())
            .map(|e| model::compose::file_draft(e.change()));
        if let Some(Ok(comment)) = draft {
            let comment = self.on_branch(comment);
            self.open_box(Compose::New(Box::new(comment)), Writes::File, &text);
            self.message(
                "the thread you were writing in is gone; this is a comment on the file now",
            );
        } else {
            self.orphaned = Some(text);
        }
    }

    /// The text of a box whose thread went where nothing else could take
    /// it, for the loop to keep in a file.
    pub(crate) fn take_orphaned(&mut self) -> Option<String> {
        self.orphaned.take()
    }

    /// A comment that's to go on the current branch gets it: one in the
    /// local review's file, which keeps branches apart.
    pub(super) fn on_branch(&self, mut comment: crate::review::Comment) -> crate::review::Comment {
        if self.comments.as_ref().is_some_and(Comments::branched) {
            comment.branch.clone_from(&self.branch);
        }
        comment
    }

    /// Whether comments can be written here; says why not.
    fn can_comment(&mut self) -> bool {
        if self.comments.is_some() {
            return true;
        }
        self.message(if self.piped {
            "a piped diff can't have comments"
        } else {
            "no comments file here: pass --comments <file>"
        });
        false
    }

    /// A new comment, without its text, on the selected lines or the line
    /// `v` would start on.
    fn new_comment(&mut self) -> Option<(Compose, Writes)> {
        let tapped = self.take_tapped();
        let target = self.doc.comment_target(tapped);
        self.doc.selection = None;
        let Some((file, side, lines)) = target else {
            self.message("no line of code here to comment on");
            return None;
        };
        let entry = &self.doc.document.files()[file];
        let Some(loaded) = entry.file().filter(|_| !entry.is_commit()) else {
            self.message("no line of code here to comment on");
            return None;
        };
        match model::compose::draft(
            entry.change(),
            model::line_diff(loaded),
            side,
            lines.clone(),
        ) {
            Ok(comment) => Some((
                Compose::New(Box::new(self.on_branch(comment))),
                Writes::New { side, lines },
            )),
            Err(why) => {
                self.message(why);
                None
            }
        }
    }

    /// `c`: the text box for a comment on the selected lines, or the line
    /// `v` would start on, right under them.
    fn comment(&mut self) {
        if !self.can_comment() {
            return;
        }
        if let Some((compose, at)) = self.new_comment() {
            self.doc.set_focus(None);
            self.open_box(compose, at, "");
        }
    }

    /// "Suggest change": [`App::comment`]'s box, holding the lines as a
    /// suggestion to edit, the cursor above it for what to say.
    fn suggest(&mut self) {
        if !self.can_comment() {
            return;
        }
        let Some((compose, at)) = self.new_comment() else {
            return;
        };
        let Compose::New(comment) = &compose else {
            return;
        };
        let lines = comment
            .anchor
            .as_ref()
            .map(|a| a.lines.join("\n"))
            .unwrap_or_default();
        let text = model::compose::with_suggestion("", Some(&lines));
        self.doc.set_focus(None);
        self.open_box(compose, at, &format!("\n\n{text}"));
        self.doc.edit_writing(|w| w.text.rewind());
    }

    /// `C`: the text box for a comment on the whole file shown, under its
    /// first row.
    fn comment_file(&mut self) {
        if !self.can_comment() {
            return;
        }
        let Some(entry) = self.doc.document.files().get(self.doc.document.pos.file) else {
            return;
        };
        if entry.is_commit() {
            self.message(if entry.is_page() {
                "the conversation is read-only here for now"
            } else {
                "a commit has no file to comment on"
            });
            return;
        }
        match model::compose::file_draft(entry.change()) {
            Ok(comment) => {
                // The box is in the diff, which a tree over it would hide.
                if self.pane == Pane::Tree {
                    self.leave_panel();
                }
                self.doc.set_focus(None);
                let comment = self.on_branch(comment);
                self.open_box(Compose::New(Box::new(comment)), Writes::File, "");
            }
            Err(why) => self.message(why),
        }
    }

    /// The command panel's "Continue in editor": your last draft in the
    /// focused thread (or the one selected in the comments panel), else a
    /// reply to it, else a new comment on the line, written in the editor.
    /// The text box is the way in from a phone: `enter` saves it and focuses
    /// its thread, and this takes it from there.
    fn comment_in_editor(&mut self) -> Option<Action> {
        if !self.can_comment() {
            return None;
        }
        let target = self.target_thread();
        if target
            .as_ref()
            .is_some_and(|t| matches!(t.at, model::At::Conversation { .. }))
        {
            self.message("the conversation is read-only here for now; o opens it on the host");
            return None;
        }
        let focused = target.map(|t| {
            let draft = t.comments.iter().rev().find(|n| !n.posted());
            (
                t.root.clone(),
                draft.map(|n| {
                    let text = model::compose::with_suggestion(&n.text, n.suggestion.as_deref());
                    (n.id.clone(), text)
                }),
            )
        });
        let (compose, text) = match focused {
            Some((_, Some((id, text)))) => (
                Compose::Edit {
                    id,
                    before: text.clone(),
                },
                text,
            ),
            Some((root, None)) => (Compose::Reply(root), String::new()),
            None => (self.new_comment()?.0, String::new()),
        };
        self.composing = Some(compose);
        self.on_save = OnSave::Nothing;
        Some(Action::Compose(text))
    }

    /// The thread the thread keys act on: the comments panel's selected
    /// one while it has the keys, else the one focused in the diff.
    pub(super) fn target_thread(&self) -> Option<model::Thread> {
        match (&self.panel, self.pane) {
            (Some(panel), Pane::Tree) => panel.thread().cloned(),
            _ => self.doc.focused().cloned(),
        }
    }

    /// A key for [`App::target_thread`]: `esc`, or one of the thread's
    /// table. `Err` for keys it doesn't take, which act as they would
    /// without a thread; `deleting` after a first [`Act::DeleteDraft`].
    pub(super) fn thread_key(
        &mut self,
        key: Key,
        mods: Mods,
        deleting: bool,
    ) -> Result<Option<Action>, ()> {
        if (key, mods) == (Key::Esc, Mods::NONE) {
            let thread = self.target_thread().ok_or(())?;
            // An ask on it stops first, the selection ends alone, and the
            // focus stays for the next `esc`.
            if self.asking.asking(&thread.root) {
                let effects = self.asking.stop(Some(&thread.root));
                return Ok(self.review_effects(effects));
            }
            if self.doc.comment_selection.take().is_none() {
                self.doc.set_focus(None);
            }
            return Ok(None);
        }
        let action = self.keys.thread_action(key, mods).ok_or(())?;
        self.thread_act(action, deleting)
    }

    /// The first key bound to `action` in the thread's table.
    pub(super) fn thread_key_name(&self, action: Act) -> Option<String> {
        (self.keys.thread_chords(action).next()).map(crate::keys::Chord::name)
    }

    /// Does `action`, of the thread's table, on [`App::target_thread`].
    /// `Err` when it doesn't act on that thread, which leaves its key to
    /// the main table.
    #[expect(clippy::too_many_lines, reason = "a dispatch: an arm per thread key")]
    pub(super) fn thread_act(&mut self, action: Act, deleting: bool) -> Result<Option<Action>, ()> {
        let thread = self.target_thread().ok_or(())?;
        let draft = thread.comments.iter().rev().find(|n| !n.posted());
        // A resolved thread with nothing on the host goes whole.
        let deletes = if thread.resolved && thread.comments.iter().all(|n| !n.posted()) {
            thread.comments.first()
        } else {
            draft
        };
        let general = matches!(thread.at, model::At::Conversation { .. });
        let mut done = None;
        match action {
            // An ask's answer is a reply.
            Act::Reply
            | Act::Resolve
            | Act::AskAgent
            | Act::ApplySuggestion
            | Act::FixWithAgent
                if general =>
            {
                let read_only = "the conversation is read-only here for now";
                self.message(match self.thread_key_name(Act::OpenOnHost) {
                    Some(key) => format!("{read_only}; {key} opens it on the host"),
                    None => read_only.to_owned(),
                });
            }
            Act::AskAgent if !self.asking.asking(&thread.root) && !self.agent_on_path() => {}
            // The box takes a question first; one ask a thread at a time.
            Act::AskAgent if !self.asking.asking(&thread.root) => {
                let root = thread.root.clone();
                done = self.write_in(
                    &thread.root,
                    Compose::Reply(root.clone()),
                    Writes::Reply(root),
                    "",
                );
                self.on_save = OnSave::Ask;
                // The box's keys say who it asks, in place of the answer's
                // news.
                self.message = None;
            }
            Act::AskAgent => self.ask_about((&thread.root, Some(thread.at.clone())), (None, None)),
            Act::ApplySuggestion => {
                done = self.apply_suggestion((&thread.root, Some(&thread.at)), None, None);
            }
            Act::FixWithAgent => self.fix(&thread.root),
            Act::Reply => {
                let warning = (self.reviewing.naming())
                    .and_then(|n| n.outdated_reply_warning.as_deref())
                    .filter(|_| thread.outdated && !self.warned_outdated);
                if let Some(warning) = warning {
                    self.warned_outdated = true;
                    self.message(format!("outdated: {}", escape(warning.as_bytes())));
                }
                let root = thread.root.clone();
                done = self.write_in(
                    &thread.root,
                    Compose::Reply(root.clone()),
                    Writes::Reply(root),
                    "",
                );
            }
            Act::EditDraft => match draft {
                Some(comment) => {
                    let text = model::compose::with_suggestion(
                        &comment.text,
                        comment.suggestion.as_deref(),
                    );
                    done = self.write_in(
                        &thread.root,
                        Compose::Edit {
                            id: comment.id.clone(),
                            before: text.clone(),
                        },
                        Writes::Edit {
                            root: thread.root.clone(),
                            id: comment.id.clone(),
                        },
                        &text,
                    );
                }
                None => self.message("no draft here to edit; posted comments are the host's"),
            },
            Act::Resolve => {
                let change = Change {
                    resolved: Some(!thread.resolved),
                    ..Change::default()
                };
                let note = if thread.resolved {
                    "thread reopened"
                } else {
                    "thread resolved"
                };
                let op = Op::Set {
                    target: Target::Id(thread.root.clone()),
                    change: Box::new(change),
                };
                self.write_comments(vec![op], note).ok();
            }
            Act::Adopt if thread.unposted() => self.adopt(&thread),
            Act::Adopt => self.message("nothing here to make yours"),
            // Text selected in a comment is the main table's `y` to copy.
            Act::CopyPrompt if self.doc.comment_selection.is_some() => return Err(()),
            Act::CopyPrompt => done = self.copy_prompt(),
            Act::DeleteDraft => match deletes {
                None => self.message("no draft here to delete; posted comments are the host's"),
                Some(comment) if deleting => {
                    let gone = if comment.id == thread.root {
                        "thread deleted"
                    } else {
                        "draft deleted"
                    };
                    (self.write_comments(vec![Op::Rm(Target::Id(comment.id.clone()))], gone)).ok();
                }
                Some(comment) => {
                    self.deleting = true;
                    let key = (self.thread_key_name(Act::DeleteDraft))
                        .unwrap_or_else(|| Act::DeleteDraft.name().to_owned());
                    self.message(if comment.id == thread.root {
                        format!(
                            "{key} again deletes this thread, replies too · any other key keeps it"
                        )
                    } else {
                        format!("{key} again deletes your draft reply · any other key keeps it")
                    });
                }
            },
            Act::OpenOnHost => match thread.url {
                Some(url) if model::text::is_web(&url) => return Ok(Some(Action::Browse(url))),
                Some(_) => self.message("this thread's link isn't a web address"),
                None => self.message("this thread has no link: it isn't posted"),
            },
            _ => return Err(()),
        }
        self.doc.comment_selection = None;
        Ok(done)
    }

    /// `a`: opens the text box for a question on the selected lines, which
    /// asks once it's saved, else asks about the thread the thread keys act
    /// on, else opens the box on the line `v` would start on.
    pub(super) fn ask_agent(&mut self) -> Option<Action> {
        let selected = self.doc.selection.is_some() && self.pane == Pane::Diff;
        if !selected && self.target_thread().is_some() {
            return self.thread_act(Act::AskAgent, false).ok().flatten();
        }
        if !self.can_comment() || !self.agent_on_path() {
            return None;
        }
        let (compose, at) = self.new_comment()?;
        self.doc.set_focus(None);
        self.open_box(compose, at, "");
        self.on_save = OnSave::Ask;
        let agent = self.asking.default_agent().unwrap_or("the agent");
        self.message(format!("your question for {agent} · enter asks"));
        None
    }

    /// Whether the agent plugin is on PATH, looked up until it's found, so
    /// one installed meanwhile counts; says so when it isn't, before a
    /// question is written for nothing to answer.
    fn agent_on_path(&mut self) -> bool {
        self.agent_found |= (self.find_agent)();
        let found = self.agent_found;
        if !found {
            self.message(format!(
                "{} isn't on PATH, so nothing can answer (see docs/plugins.md)",
                crate::plugin::agent::PROGRAM
            ));
        }
        found
    }

    /// `fix_with_agent`: on the selected lines, opens the text box for a
    /// note on them, which hands its thread to your coding agent once
    /// saved; else hands it the thread the thread keys act on.
    pub(super) fn fix_with_agent(&mut self) -> Option<Action> {
        let selected = self.doc.selection.is_some() && self.pane == Pane::Diff;
        if !selected && self.target_thread().is_some() {
            return self.thread_act(Act::FixWithAgent, false).ok().flatten();
        }
        if !selected {
            self.message("fix with agent works on a thread, or on selected lines");
            return None;
        }
        if !self.can_comment() {
            return None;
        }
        let (compose, at) = self.new_comment()?;
        self.doc.set_focus(None);
        self.open_box(compose, at, "");
        self.on_save = OnSave::Fix;
        self.message("what should change here · enter sends it and starts your agent");
        None
    }

    /// Hands the thread with first comment `root` to the user's coding
    /// agent, which the loop starts once a frame is out.
    fn fix(&mut self, root: &str) {
        let Some(comments) = &self.comments else {
            return;
        };
        let (file, session) = comments.file_with(root);
        self.handoff = Some(Handoff {
            what: Handed::Thread(root.to_owned()),
            file: file.to_owned(),
            session,
            checkout: self.checkout(),
        });
    }

    /// How many open threads of yours there are to send.
    pub(super) fn open_threads(&self) -> usize {
        let (file, local) =
            (self.comments.as_ref()).map_or((0, 0), Comments::open_threads_of_yours);
        file + local
    }

    /// `send_open_threads`: hands every open thread of yours to your
    /// coding agent at once, which the loop starts once a frame is out,
    /// taking a checkpoint first as for one thread. The agent reads one
    /// comments file: on a review, of the review's file and the local
    /// review's drafts beside it, the one with more of yours, or as many,
    /// the one new threads go to; the other's are counted as left.
    pub(super) fn send_open_threads(&mut self) {
        let Some(comments) = self.comments.as_ref().filter(|_| self.open_threads() > 0) else {
            self.message("no open threads of yours to send");
            return;
        };
        let (in_file, in_local) = comments.open_threads_of_yours();
        let local_first = in_local > in_file || (in_local == in_file && comments.branched());
        let ((file, session), count, left) = match comments.local_file() {
            Some(local) if local_first => ((local, false), in_local, in_file),
            _ => (comments.file(), in_file, in_local),
        };
        self.handoff = Some(Handoff {
            what: Handed::Open { count, left },
            file: file.to_owned(),
            session,
            checkout: self.checkout(),
        });
    }

    /// The threads handed to the user's coding agent, for the loop.
    pub(crate) fn take_handoff(&mut self) -> Option<Handoff> {
        self.handoff.take()
    }

    /// The agent the loop handed threads to started, or why not; `left`,
    /// when it was handed your open threads, is how many of yours in the
    /// other comments file it wasn't. The checkpoint taken before it shows
    /// its turn.
    pub(crate) fn handed_off(&mut self, started: Result<String, String>, left: Option<usize>) {
        match (started, left) {
            (Ok(mut note), Some(left)) => {
                let since = self.checkpoint_key(Act::SinceCheckpoint);
                let _ = write!(note, " · {since} shows its turn");
                if left > 0 {
                    let _ = write!(
                        note,
                        " · {left} more of yours in the other comments file not sent"
                    );
                }
                self.message(note);
            }
            (Ok(mut note), None) => {
                let watch = self.key_for(false, Act::ToggleWatch);
                if let (Watch::Off, Some(key)) = (&self.watch, watch) {
                    let _ = write!(note, " · {key} shows its edits live");
                }
                self.message(note);
            }
            (Err(why), _) => self.failed(why),
        }
    }

    /// The review session checked out, when changing the working tree for
    /// it needs HEAD on its head: one opened by its id. The review plain
    /// `dffy` found is the branch's own, checked out already.
    pub(super) fn checkout(&self) -> Option<Checkout> {
        let mode = self.reviewing.mode.as_ref().filter(|m| m.plain.is_none())?;
        Some(Checkout {
            label: mode.label.clone(),
            head: mode.session.head.clone(),
            command: mode.session.checkout.clone(),
        })
    }

    /// `s`: puts a suggestion of the thread with first comment `root`,
    /// placed `at` on its diff when it is, in place of its lines in the
    /// working tree: comment `id`'s, else the comment's last clicked when
    /// it has one, else its last comment's with one. One applied already
    /// is taken back, putting the lines it replaced in place again. `reply`,
    /// a control API request, is answered with what came of it; that only
    /// applies, so one client can't undo what another applied.
    pub(super) fn apply_suggestion(
        &mut self,
        (root, at): (&str, Option<&model::At>),
        id: Option<&str>,
        reply: Option<Reply>,
    ) -> Option<Action> {
        let picked = if self.editing {
            Err(STILL_EDITING.to_owned())
        } else {
            self.suggestion((root, at), id, reply.is_none())
        };
        let picked = picked.and_then(|picked| {
            if !picked.stale.is_empty() {
                self.write_comments(picked.stale.clone(), "its suggestion isn't in the file now")?;
            }
            Ok(picked)
        });
        match picked {
            Ok(Picked {
                suggestion,
                replaced,
                ..
            }) => {
                self.applying = Some(Applying {
                    thread: suggestion.thread.clone(),
                    comment: suggestion.comment.clone(),
                    path: String::from_utf8_lossy(&suggestion.diff.change.path).into_owned(),
                    lines: suggestion.lines.len(),
                    replaced,
                    reply,
                });
                Some(Action::ApplySuggestion(Box::new(suggestion)))
            }
            Err(why) => {
                match reply {
                    Some(reply) => reply.send(Err(Fault::new("failed", why))),
                    None => self.message(why),
                }
                None
            }
        }
    }

    /// What `s` does with the thread's suggestion: applies it, or with
    /// `toggle`, takes one applied already back. On the working tree's
    /// diff, an applied suggestion whose lines aren't where the thread is,
    /// as after a discard, applies again from the lines it replaced.
    fn suggestion(
        &self,
        (root, at): (&str, Option<&model::At>),
        id: Option<&str>,
        toggle: bool,
    ) -> Result<Picked, String> {
        let comments = self.comments.as_ref().ok_or("no comments file here")?;
        let thread = comments.thread(root);
        let clicked = id.or_else(|| {
            (self.tapped_comment.as_ref())
                .filter(|(tapped, _)| tapped == root)
                .map(|(_, id)| id.as_str())
        });
        let has = |c: &&&crate::review::Comment| c.suggestion.is_some();
        let picked = (clicked.and_then(|id| thread.iter().find(|c| c.id == id)))
            .filter(has)
            .or_else(|| thread.iter().rev().find(has))
            .ok_or("this thread has no suggestion to apply")?;
        if picked.applied.is_some() && !toggle {
            return Err("this suggestion is applied already".to_owned());
        }
        let first = thread.first().copied();
        let (Some(anchor), Some((side, written))) = (
            first.and_then(|c| c.anchor.clone()),
            first.and_then(crate::review::Comment::range),
        ) else {
            return Err("this thread is on its whole file: a suggestion replaces lines".to_owned());
        };
        if side == crate::review::Side::Old {
            return Err(
                "this thread is on the old version's lines, which the working tree doesn't have"
                    .to_owned(),
            );
        }
        let at = match at {
            Some(model::At::Line {
                side: Side::New,
                lines,
            }) => lines.start,
            _ => written.start,
        };
        let file = (comments.home_of(root))
            .and_then(|path| comments.file_of(path.as_bytes()))
            .ok_or("its file isn't in this diff")?;
        let diff = (self.doc.document.file(file))
            .map(model::line_diff)
            .ok_or("its file's diff isn't loaded yet")?;
        // Writers other than dffy may end lines with CRLF, or end the last
        // line too; empty is no lines, which deletes them.
        let text = (picked.suggestion.as_deref().unwrap_or_default()).replace("\r\n", "\n");
        let lines: Vec<String> = if text.is_empty() {
            Vec::new()
        } else {
            let text = text.strip_suffix('\n').unwrap_or(&text);
            text.split('\n').map(str::to_owned).collect()
        };
        let suggestion = Suggestion {
            diff: Arc::clone(diff),
            at,
            anchor,
            lines,
            checkout: self.checkout(),
            thread: root.to_owned(),
            comment: picked.id.clone(),
        };
        if picked.applied.is_some() {
            return take_back(suggestion, picked.replaced.as_deref());
        }
        Ok(Picked {
            replaced: Some(without_cr(&suggestion.anchor.lines)),
            suggestion,
            stale: Vec::new(),
        })
    }

    /// What came of the suggestion the loop applied: the comment marked
    /// applied and its thread moved to the new lines when the diff shows
    /// them, said, and answered to the control API when it asked.
    pub(crate) fn applied(&mut self, result: Result<annotation::Applied, String>) {
        self.editing = false;
        let Some(applying) = self.applying.take() else {
            return;
        };
        match &result {
            Ok(applied) => self.mark_applied(&applying, applied),
            Err(why) => self.failed(why.to_owned()),
        }
        if let Some(reply) = applying.reply {
            reply.send(match result {
                Ok(applied) => Ok(crate::control::viewer::applied(
                    (applying.thread, applying.comment),
                    applying.path,
                    applied.replaced,
                    applying.lines,
                )),
                Err(why) => Err(Fault::new("failed", why)),
            });
        }
    }

    /// Marks `applying`'s comment applied, keeping what it replaced, or no
    /// longer applied once taken back, and moves its thread to the lines
    /// `applied` put there, when it has them anchored, saying so. The thread
    /// stays open: resolving it is the user's.
    fn mark_applied(&mut self, applying: &Applying, applied: &annotation::Applied) {
        let lines = |r: &Range<usize>| match r.len() {
            0 | 1 => format!("{}:{}", applying.path, r.start + 1),
            _ => format!("{}:{}-{}", applying.path, r.start + 1, r.end),
        };
        let mut note = match &applying.replaced {
            None => format!(
                "took the suggestion back: {} as before",
                lines(&applied.lines)
            ),
            Some(_) if applying.lines == 0 => {
                format!("deleted {}, as suggested", lines(&applied.replaced))
            }
            Some(_) => format!("applied the suggestion to {}", lines(&applied.lines)),
        };
        let open = (self.comments.as_ref()).and_then(|c| {
            c.thread(&applying.thread)
                .first()
                .map(|root| !root.resolved)
        });
        if applying.replaced.is_some()
            && open == Some(true)
            && let Some(key) = self.thread_key_name(Act::Resolve)
        {
            let _ = write!(note, " · {key} resolves");
        }
        let marked = match &applying.replaced {
            Some(replaced) => Change {
                applied: Some(Some(crate::review::timestamp(SystemTime::now()))),
                replaced: Some(Some(replaced.join("\n"))),
                ..Change::default()
            },
            None => Change {
                applied: Some(None),
                ..Change::default()
            },
        };
        let mut ops = vec![Op::Set {
            target: Target::Id(applying.comment.clone()),
            change: Box::new(marked),
        }];
        if let Some(anchor) = &applied.anchor {
            let at = |line: usize| crate::review::LineNumbers {
                old: None,
                new: Some(line),
            };
            ops.push(Op::Set {
                target: Target::Id(applying.thread.clone()),
                change: Box::new(Change {
                    anchor: Some(anchor.clone()),
                    start: Some(at(applied.lines.start)),
                    end: Some(at(applied.lines.end - 1)),
                    start_side: Some(None),
                    ..Change::default()
                }),
            });
        }
        let note = escape(note.as_bytes());
        if let Err(why) = self.write_comments(ops, &note) {
            self.message(if applying.replaced.is_some() {
                format!("{note}, but it isn't marked applied: {why}")
            } else {
                format!("{note}, but it's still marked applied: {why}")
            });
        }
    }

    /// Asks `agent`, or the default one, about the thread with first
    /// comment `root`, shown `at` when that's known, as a prompt made now;
    /// `mention` is the comment whose `@<name>` asks.
    fn ask_about(
        &mut self,
        (root, at): (&str, Option<model::At>),
        (agent, mention): (Option<String>, Option<String>),
    ) {
        let made = match self.root_prompt(root, at) {
            Some(Ok(made)) => made,
            Some(Err(why)) => {
                self.message(why);
                return;
            }
            None => return,
        };
        let effects = self
            .asking
            .ask(root.to_owned(), made.text, (agent, mention));
        self.review_effects(effects);
    }

    /// The agent whose answer ends the thread with first comment `root`:
    /// its last comment, when that has no ref and is by an agent the
    /// plugin listed. Until it has, no thread ends in an answer.
    pub(super) fn answered_by(&self, root: &str) -> Option<String> {
        let last = *self.comments.as_ref()?.thread(root).last()?;
        (last.reference.is_none() && self.asking.lists(&last.author)).then(|| last.author.clone())
    }

    /// Who `enter` in the text box asks, when it's to ask: the agent a
    /// follow-up answers, else the default one, once the plugin named it.
    pub(super) fn asks_whom(&self) -> Option<String> {
        let answered = match self.doc.writing().map(|w| &w.at) {
            Some(Writes::Reply(root)) => self.answered_by(root),
            _ => None,
        };
        answered.or_else(|| self.asking.default_agent().map(str::to_owned))
    }

    /// The thread with first comment `root`, shown `at`, as a prompt; without
    /// `at`, where the diff shows it, or for one just written and not placed
    /// yet, on its own lines.
    fn root_prompt(
        &self,
        root: &str,
        at: Option<model::At>,
    ) -> Option<Result<prompt::Prompt, &'static str>> {
        let placed = (self.doc.threads().iter()).find(|t| t.root == root);
        let at = if let Some(at) = at.or_else(|| placed.map(|t| t.at.clone())) {
            at
        } else {
            let comments = self.comments.as_ref()?;
            let first = comments.thread(root).first().copied()?;
            match first.range() {
                Some((side, lines)) => model::At::Line {
                    side: match side {
                        crate::review::Side::Old => Side::Old,
                        crate::review::Side::New => Side::New,
                    },
                    lines,
                },
                None => model::At::File,
            }
        };
        Some(self.thread_prompt(root, &at)?.map(|(made, _)| made))
    }

    /// `clear_resolved`: asks before deleting the resolved threads shown,
    /// naming what goes ([`Comments::clear_resolved`] has which).
    pub(super) fn clear_resolved(&mut self) {
        let Some(comments) = &self.comments else {
            self.message("no comments file here");
            return;
        };
        if self.writes_held() {
            self.message(POSTING);
            return;
        }
        let cleared = comments.clear_resolved();
        if cleared.threads + cleared.comments == 0 {
            self.message(with_kept("no resolved threads to delete", &cleared));
            return;
        }
        self.message = None;
        let prompt = format!("delete {}? y/n", cleared_words(&cleared));
        self.confirm = Some((prompt, Confirmed::Clear(cleared), "kept"));
    }

    /// Deletes what [`App::clear_resolved`] asked about, in one write per
    /// file it's in.
    pub(super) fn cleared(&mut self, cleared: model::Cleared) {
        let note = with_kept(&format!("deleted {}", cleared_words(&cleared)), &cleared);
        for ops in cleared.ops.into_iter().filter(|ops| !ops.is_empty()) {
            if self.write_comments(ops, &note).is_err() {
                return;
            }
        }
    }

    /// `A`: someone else's comments in `thread`, an agent's, become yours:
    /// drafts that posting sends under your name.
    fn adopt(&mut self, thread: &model::Thread) {
        let ops: Vec<Op> = (thread.comments.iter())
            .filter(|n| n.standing == model::Standing::Unposted)
            .map(|n| Op::Set {
                target: Target::Id(n.id.clone()),
                change: Box::new(Change {
                    author: Some(String::new()),
                    ..Change::default()
                }),
            })
            .collect();
        let note = format!("now yours: {}", session::plural(ops.len(), "draft"));
        self.write_comments(ops, &note).ok();
    }

    /// `V`: the thread the thread keys act on, on another branch, moves to
    /// the current one.
    fn move_here(&mut self) {
        let Some(thread) = self.target_thread() else {
            self.message("no thread here to move; m goes to the next");
            return;
        };
        if thread.branch.is_none() {
            self.message("this thread is on this branch already");
            return;
        }
        let op = Op::Set {
            target: Target::Id(thread.root),
            change: Box::new(Change {
                branch: Some(self.branch.clone()),
                ..Change::default()
            }),
        };
        let note = match &self.branch {
            Some(branch) => format!("moved to {}", escape(branch.as_bytes())),
            None => "moved to no branch: it shows on every one".to_owned(),
        };
        self.write_comments(vec![op], &note).ok();
    }

    /// Opens the text box `at` in the thread with root `root` when the
    /// diff shows that thread, else hands `text` to the editor: the box is
    /// drawn only inside its thread in the diff. From the comments panel
    /// the diff follows the thread first, and takes the keys for the box.
    fn write_in(&mut self, root: &str, compose: Compose, at: Writes, text: &str) -> Option<Action> {
        let shown = |app: &Self| app.doc.focused().is_some_and(|t| t.root == root);
        if self.pane == Pane::Tree && self.panel.is_some() {
            if !shown(self) {
                self.follow_thread(root);
            }
            if shown(self) {
                self.leave_panel();
            }
        }
        if shown(self) {
            self.open_box(compose, at, text);
            return None;
        }
        self.composing = Some(compose);
        self.on_save = OnSave::Nothing;
        Some(Action::Compose(text.to_owned()))
    }

    pub(super) fn comment_action(&mut self, action: Act) -> Option<Action> {
        match action {
            Act::Comment => self.comment(),
            Act::SuggestChange => self.suggest(),
            Act::CommentFile => self.comment_file(),
            Act::Comments => self.toggle_comments(),
            Act::NextThread | Act::PrevThread => {
                self.doc.selection = None;
                self.step_thread(action == Act::NextThread);
            }
            Act::ToggleThread => self.toggle_thread(),
            Act::ToggleThreads => self.toggle_threads(),
            Act::MoveThread => self.move_here(),
            Act::CommentInEditor => return self.comment_in_editor(),
            Act::CommentedFiles => self.toggle_commented(),
            _ => {}
        }
        None
    }

    /// `m` and `M`: the next or previous thread, on into the next or
    /// previous file with comments, focused.
    fn step_thread(&mut self, forward: bool) {
        let Some(comments) = &self.comments else {
            self.message("no comments file here: pass --comments <file>");
            return;
        };
        // The thread `enter` just saved, as its note says, when this is the
        // key right after.
        let saved = self.just_saved.take();
        if let Some((root, _)) = saved.filter(|(root, at)| {
            forward && self.inputs == at + 1 && comments.thread_root(root).is_some()
        }) {
            self.follow_thread(&root);
            return;
        }
        // An update's new threads come first, in list order, while any of
        // them is still there.
        while forward && !self.new_threads.is_empty() {
            let root = self.new_threads.remove(0);
            if comments.thread_root(&root).is_none() {
                continue;
            }
            let left = self.new_threads.len();
            self.open_thread(&root);
            if left > 0 && self.message.is_none() {
                self.message(format!("{left} more new"));
            }
            return;
        }
        // The shown file's blocks are given as they're measured; the core
        // steps into the others by these.
        self.doc.document.set_held_blocks(&comments.files());
        let stepped = match self.doc.step_thread(forward) {
            Stepped::Nowhere if forward => {
                self.next_unlisted().map_or(Stepped::Nowhere, Stepped::File)
            }
            stepped => stepped,
        };
        let file = match stepped {
            Stepped::Thread(_) => return,
            Stepped::File(file) => file,
            Stepped::Nowhere => {
                self.message(if forward {
                    "no more threads below"
                } else {
                    "no more threads above"
                });
                return;
            }
        };
        let here = self.doc.document.pos.file;
        self.doc.set_focus(None);
        self.doc.goto_file(file);
        self.sync_threads();
        if !self.doc.edge_thread(!forward) {
            self.thread_pending = Some((file, Seek::Edge(!forward)));
            self.wait_note(file);
            return;
        }
        let (arrow, what) = if forward {
            ("→", "next file")
        } else {
            ("←", "previous file")
        };
        self.note_file(arrow, what, here, file);
    }

    /// `z`: folds the focused thread, or the first on screen, or unfolds
    /// it.
    fn toggle_thread(&mut self) {
        let Some(i) = self.doc.nearest_thread() else {
            self.message("no thread here; m goes to the next");
            return;
        };
        let root = self.doc.threads()[i].root.clone();
        self.doc.toggle_fold(&root);
    }

    /// `Z`: every thread folded, or every one unfolded.
    fn toggle_threads(&mut self) {
        let folded = self.doc.toggle_folds();
        self.message(if folded {
            "threads folded; z unfolds one"
        } else {
            "threads unfolded"
        });
    }

    /// The tree keeps only the files with comments, or every file again.
    fn toggle_commented(&mut self) {
        let Some(comments) = &self.comments else {
            self.message("no comments file here: pass --comments <file>");
            return;
        };
        let on = !self.tree.commented();
        self.tree.set_commented(on.then(|| comments.paths()));
        self.close_panel();
        if on {
            self.focus_tree();
            if let Some(Item::File(file)) = self.tree.step(0) {
                self.doc.goto_file(file);
            }
        }
        self.message(if on {
            "the tree shows files with comments; f/F and n/N keep to them"
        } else {
            "the tree shows every file"
        });
    }

    /// `i`: the left panel shows every comment thread, with the keys,
    /// starting on the focused one or the first of the file shown; or,
    /// when it shows them, the files again.
    fn toggle_comments(&mut self) {
        if self.panel.is_some() && self.tree_shown {
            self.close_panel();
            return;
        }
        if self.panel.is_none() {
            if !self.can_comment() {
                return;
            }
            self.note_gone();
            let Some(comments) = &self.comments else {
                return;
            };
            let root = self.doc.focused().map(|t| t.root.as_str());
            let file = self.doc.document.files().get(self.doc.document.pos.file);
            let path = file.map_or(&[][..], |e| &e.change().path[..]);
            let opens = self.worktree.is_some() && !self.piped;
            self.panel = Some(Panel::new(
                comments,
                (self.enumerating, opens),
                (root, path),
            ));
        }
        self.focus_tree();
    }

    /// Closes the comments panel, ending the peek its selection made.
    /// Returns whether it was open.
    pub(super) fn close_panel(&mut self) -> bool {
        self.doc.peek(None);
        self.panel.take().is_some()
    }

    /// A key for the comments panel, when it has the keys: the thread keys
    /// act on the selected thread, as on a focused one in the diff (see
    /// [`App::thread_key`]). `Continue` when it's neither's.
    pub(super) fn panel_key(
        &mut self,
        key: Key,
        mods: Mods,
        deleting: bool,
    ) -> ControlFlow<Option<Action>> {
        let (Some(panel), Some(comments), Pane::Tree) =
            (&mut self.panel, &self.comments, self.pane)
        else {
            return ControlFlow::Continue(());
        };
        match panel.key((comments, self.enumerating), &self.keys, key, mods) {
            comments::Outcome::Stay => {}
            comments::Outcome::Moved(root) => self.follow_thread(&root),
            comments::Outcome::Diff => self.leave_panel(),
            comments::Outcome::Open => {
                let root = panel.selected().map(str::to_owned);
                let unlisted = root.filter(|root| self.unlisted_path(root).is_some());
                if unlisted.is_none_or(|root| self.open_thread(&root)) {
                    self.leave_panel();
                }
            }
            comments::Outcome::Pass => {
                return match self.thread_key(key, mods, deleting) {
                    Ok(done) => ControlFlow::Break(done),
                    Err(()) => ControlFlow::Continue(()),
                };
            }
        }
        ControlFlow::Break(None)
    }

    /// Shows the thread with root `root` in the diff, focused and peeked
    /// at, its file loaded when it's another, and leaves the keys where
    /// they are; a thread not placed yet is focused once it is. One on a
    /// file not in the diff leaves nothing focused: the panel's header
    /// says so.
    pub(super) fn follow_thread(&mut self, root: &str) {
        // A `d` armed on the thread selected before would delete in this one.
        self.deleting = false;
        let Some(comments) = &self.comments else {
            return;
        };
        // On another branch: only the panel lists it.
        if comments.elsewhere(root) {
            self.doc.peek(None);
            self.doc.set_focus(None);
            return;
        }
        let path = comments.home_of(root).unwrap_or_default().to_owned();
        self.doc.selection = None;
        self.thread_pending = None;
        self.doc.peek(Some(root));
        // A general comment is on the conversation page, the first entry.
        let page = (self.doc.document.files().first())
            .is_some_and(view::Entry::is_page)
            .then_some(0);
        let Some(file) = (if path.is_empty() {
            page
        } else {
            comments.file_of(path.as_bytes())
        }) else {
            self.doc.set_focus(None);
            if self.enumerating {
                self.message("still listing files; its file isn't here yet");
            }
            return;
        };
        self.doc.goto_file(file);
        self.sync_threads();
        if !self.doc.go_to_thread(root) {
            self.doc.set_focus(None);
            self.thread_pending = Some((file, Seek::Root(root.to_owned())));
            self.wait_note(file);
        }
    }

    /// [`App::follow_thread`], adding the thread's file first when the diff
    /// doesn't list it (see [`App::unchanged_entry`]). When that can't be,
    /// says why and returns false.
    pub(super) fn open_thread(&mut self, root: &str) -> bool {
        if let Some(path) = self.unlisted_path(root)
            && let Err(why) = self.unchanged_entry(&path)
        {
            self.follow_thread(root);
            self.message(why);
            return false;
        }
        self.follow_thread(root);
        true
    }

    /// The path of the thread with root `root` when it's on a file the
    /// diff doesn't list; not for a general comment or one on another
    /// branch, which have no file to open.
    pub(super) fn unlisted_path(&self, root: &str) -> Option<String> {
        let comments = self.comments.as_ref().filter(|c| !c.elsewhere(root))?;
        let path = comments.home_of(root).filter(|p| !p.is_empty())?;
        comments
            .file_of(path.as_bytes())
            .is_none()
            .then(|| path.to_owned())
    }

    /// The entry of `path`, a commented file: the diff's own, else one
    /// added for it after the others, as the working tree has it, whole and
    /// with no changes (see [`Source::Unchanged`]); built once it shows.
    /// Says why there's none to add: the list is still growing, there's no
    /// working tree, or the file isn't in it, which the panel then says.
    fn unchanged_entry(&mut self, path: &str) -> Result<usize, String> {
        let comments = self.comments.as_mut().ok_or("no comments file here")?;
        if let Some(file) = comments.file_of(path.as_bytes()) {
            return Ok(file);
        }
        if self.enumerating {
            return Err("still listing files; its file isn't here yet".to_owned());
        }
        let shown = escape(path.as_bytes());
        let root = (self.worktree.as_deref())
            .filter(|_| !self.piped)
            .ok_or_else(|| format!("{shown} isn't in this diff"))?;
        let Some(mode) = dffy_core::git::worktree_file(root, path.as_bytes()) else {
            comments.set_gone(path.as_bytes(), true);
            return Err(format!("{shown} isn't on disk"));
        };
        comments.set_gone(path.as_bytes(), false);
        let change = dffy_core::git::FileChange::unchanged(path.into(), mode);
        let file = self.doc.document.show_unchanged(change);
        self.rebuild_tree();
        if let Some(picker) = &mut self.picker {
            picker.rebuild(self.doc.document.files());
        }
        self.dirty = true;
        Ok(file)
    }

    /// Notes which commented files the diff doesn't list are missing from
    /// the working tree, for the panel to say before one is opened.
    fn note_gone(&mut self) {
        let (Some(comments), Some(root)) = (&mut self.comments, &self.worktree) else {
            return;
        };
        let paths: Vec<String> = (comments.unlisted().into_iter())
            .map(str::to_owned)
            .collect();
        for path in paths {
            let gone = dffy_core::git::worktree_file(root, path.as_bytes()).is_none();
            comments.set_gone(path.as_bytes(), gone);
        }
    }

    /// The entry `m` goes on to past the last file: the first file with
    /// threads the diff doesn't list that's on disk, added (see
    /// [`App::unchanged_entry`]).
    fn next_unlisted(&mut self) -> Option<usize> {
        let paths: Vec<String> = (self.comments.as_ref()?.unlisted().into_iter())
            .map(str::to_owned)
            .collect();
        paths
            .iter()
            .find_map(|path| self.unchanged_entry(path).ok())
    }

    /// Says what a thread going to `file` waits for, unless it's only its
    /// placement, which comes right after.
    fn wait_note(&mut self, file: usize) {
        if self.doc.document.file(file).is_none() {
            self.message("waiting for this file's diff");
        }
    }

    /// The comments' annotations, for the pool to place on the diffs it
    /// makes, when they changed since last taken.
    pub(crate) fn take_annotations(&mut self) -> Option<AnnotationSet> {
        if !std::mem::take(&mut self.annotations_changed) {
            return None;
        }
        Some(self.comments.as_ref()?.annotations().clone())
    }
}

/// What `s` is to do: the suggestion for the loop to apply, the lines it
/// replaces when it's being applied (`None` when it's taken back), and the
/// writes that clear an applied mark the file no longer bears out.
struct Picked {
    suggestion: Suggestion,
    replaced: Option<Vec<String>>,
    stale: Vec<Op>,
}

/// What `s` does with `applied`, a suggestion applied already, as it would
/// apply now, its `replaced` lines kept on its comment: puts them back in
/// place of the suggested ones. On the working tree's diff, a suggestion
/// whose lines aren't where the thread is, as after a discard, isn't
/// applied any more: its mark is cleared and it applies again from them.
fn take_back(applied: Suggestion, replaced: Option<&str>) -> Result<Picked, String> {
    let Some(old) = replaced else {
        return Err(
            "this suggestion is applied already, and what it replaced wasn't kept to put back"
                .to_owned(),
        );
    };
    if applied.lines.is_empty() {
        return Err(
            "this suggestion deleted its lines, so nothing marks where they go back; \
             D or your editor brings them back"
                .to_owned(),
        );
    }
    let old: Vec<String> = old.split('\n').map(str::to_owned).collect();
    // The lines around are the same before and after it's applied. An
    // anchor's lines keep a CRLF ending's `\r`, which the thread's, its
    // lines as they are now in either version, tell; the write gives the
    // new lines the ending of those they replace.
    let crlf = (applied.anchor.lines.last()).is_some_and(|l| l.ends_with('\r'));
    let around = |lines: &[String]| Anchor {
        before: applied.anchor.before.clone(),
        lines: (lines.iter())
            .map(|l| if crlf { format!("{l}\r") } else { l.clone() })
            .collect(),
        after: applied.anchor.after.clone(),
    };
    let suggested = around(&applied.lines);
    let worktree = matches!(
        applied.diff.change.new,
        Some(dffy_core::git::Side::Worktree { .. })
    );
    if worktree && suggested.place(&applied.diff.new, applied.at).is_none() {
        let original = around(&old);
        let set = |id: &str, change: Change| Op::Set {
            target: Target::Id(id.to_owned()),
            change: Box::new(change),
        };
        let line = |at: usize| crate::review::LineNumbers {
            old: None,
            new: Some(at),
        };
        let stale = vec![
            set(
                &applied.comment,
                Change {
                    applied: Some(None),
                    ..Change::default()
                },
            ),
            set(
                &applied.thread,
                Change {
                    anchor: Some(original.clone()),
                    start: Some(line(applied.at)),
                    end: Some(line(applied.at + old.len() - 1)),
                    ..Change::default()
                },
            ),
        ];
        return Ok(Picked {
            suggestion: Suggestion {
                anchor: original,
                ..applied
            },
            replaced: Some(old),
            stale,
        });
    }
    Ok(Picked {
        suggestion: Suggestion {
            anchor: suggested,
            lines: old,
            ..applied
        },
        replaced: None,
        stale: Vec::new(),
    })
}

/// `lines` as an anchor holds them, without a CRLF ending's `\r`, as a
/// suggestion's are.
fn without_cr(lines: &[String]) -> Vec<String> {
    (lines.iter())
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned())
        .collect()
}

/// How the text written was sent: `enter` in the box, `esc` (or anything
/// else closing it), or the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum By {
    Enter,
    Esc,
    Editor,
}

/// What clearing the resolved threads removes: `3 resolved threads and 1
/// private comment in host threads`.
fn cleared_words(cleared: &model::Cleared) -> String {
    let threads =
        (cleared.threads > 0).then(|| session::plural(cleared.threads, "resolved thread"));
    let comments = (cleared.comments > 0).then(|| {
        format!(
            "{} in host threads",
            session::plural(cleared.comments, "private comment")
        )
    });
    threads
        .into_iter()
        .chain(comments)
        .collect::<Vec<_>>()
        .join(" and ")
}

/// `said`, and why the host's resolved threads stay when there are some.
fn with_kept(said: &str, cleared: &model::Cleared) -> String {
    if cleared.kept == 0 {
        return said.to_owned();
    }
    format!("{said} · host threads stay; resolve hides them")
}
