//! Drawing a frame of the viewer: the chrome, the status bar's words, and
//! the overlays over the diff, from the state the rest of `app` keeps.

use std::fmt::Write as _;

use unicode_width::UnicodeWidthStr;

use dffy_core::document::{Side, Source, State};
use dffy_core::git::Status;
use dffy_core::view::Attributes;

use super::{App, OnSave, Pane, Watch};
use crate::config::StatusBar;
use crate::keys::Action as Act;
use crate::render::escape;
use crate::review::model::{self, Comments};
use crate::review::session;
use crate::theme::{Role, Style, Theme};
use crate::tui::chrome::{self, Chrome, Cut, Heading, Switcher, Title};
use crate::tui::picker::{self, Picker};
use crate::tui::piped;
use crate::tui::tree::Totals;
use crate::tui::view::{self, Doc};
use crate::{print, term};

impl App {
    /// What an empty diff pane says: as printed for an empty diff, and
    /// whether it's watching for changes.
    fn empty_note(&self) -> String {
        if self.enumerating {
            return "listing changes…".to_owned();
        }
        let text = match &self.selected {
            Some((revisions, default)) => print::empty_text(revisions, *default, self.others),
            None => "no changes".to_owned(),
        };
        if self.watch == Watch::Off {
            text
        } else {
            format!("{text} — watching")
        }
    }

    /// Where the view is, for the title: the scope line, the definitions
    /// around the top line (`impl Error › fn fatal`), where the file's
    /// language has them; otherwise the hunk's heading, `@@ fn name()`,
    /// or with hunk headers off the heading alone. With neither, the
    /// hunk's numbers.
    fn context(&mut self) -> Option<(chrome::Spans, Cut)> {
        let scope = self.doc.scope();
        if let Some(scope) = scope.as_ref().filter(|s| !s.is_empty()) {
            return Some((vec![(scope.join(" › "), Role::Dim)], Cut::Left));
        }
        let context = self.doc.context()?;
        // At the top level the scope line is empty: the heading git
        // would find above is another definition's.
        let heading = if scope.is_some() {
            String::new()
        } else {
            context.heading
        };
        let spans = match context.numbers {
            Some(numbers) if self.hunk_headers && heading.is_empty() => {
                vec![(numbers, Role::HunkHeader)]
            }
            Some(_) if self.hunk_headers => {
                vec![("@@ ".to_owned(), Role::HunkHeader), (heading, Role::Dim)]
            }
            _ if heading.is_empty() => return None,
            _ => vec![(heading, Role::Dim)],
        };
        Some((spans, Cut::Right))
    }

    fn chrome(&mut self) -> Chrome {
        let narrow = !self.side_tree();
        let note = note(self);
        let doc = &self.doc;
        let extra = doc
            .document
            .files()
            .get(doc.document.pos.file)
            .and_then(|e| match &e.state() {
                State::Loaded(l) if l.diff().note.is_some() => l.diff().note.clone(),
                State::Loaded(l) if doc.document.structural => {
                    l.structural_note().map(str::to_owned)
                }
                _ => None,
            })
            .or_else(|| self.render_note());
        let status = chrome::Status {
            chip: chip(self),
            position: position(self).filter(|_| !narrow),
            hunk: doc
                .document
                .hunk_position()
                .map(|(i, n)| format!("{i}/{n} hunk")),
            middle: note.clone().or_else(|| Some(modes(self))),
            note: note.is_some(),
            review: review_status(self),
            search: [
                self.search_status(),
                (!self.tree.filter().is_empty() || self.tree.commented())
                    .then(|| "filter".to_owned()),
                // While the conflict's keys come first.
                self.has_conflicts().then(|| "conflicts".to_owned()),
            ]
            .into_iter()
            .flatten()
            .reduce(|a, b| format!("{a} {b}")),
            extra,
            hints: hints(self, narrow),
        };
        // The picker covers the tree, which isn't drawn under it.
        let tree = self.picker.is_none();
        Chrome {
            framed: self.framed(),
            status_top: self.status_bar == StatusBar::Top,
            tree: self.side_width().filter(|_| tree),
            overlay: self.tree_shown && narrow && tree,
            tree_editing: self.tree.editing || self.panel.as_ref().is_some_and(|p| p.editing),
            heading: heading(self),
            context: self.context(),
            status,
        }
    }

    /// The bytes for the terminal when something changed since the last
    /// frame.
    #[expect(
        clippy::too_many_lines,
        reason = "the frame's layers in the order they're drawn"
    )]
    pub(crate) fn draw(&mut self) -> Option<&[u8]> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        self.frame += 1;
        self.update_render();
        let (width, height) = (self.screen.width(), self.screen.height());
        let x = self.content_x();
        let tree_columns = self.tree_columns();
        let framed = self.framed();
        let search_line = self.search.as_ref().filter(|s| s.editing).map(|s| {
            let file = self.doc.document.pos.file;
            let here = match &self.doc.document.search {
                Some(m) if m.file == file => m.matches.len().to_string(),
                _ if s.needle.text.is_empty() => "0".to_owned(),
                _ => "…".to_owned(),
            };
            (
                format!("/{}", s.needle.text),
                format!("  {here} in this file · enter goes · esc cancels"),
            )
        });
        let chrome = self.chrome();
        let empty = self
            .doc
            .document
            .files()
            .is_empty()
            .then(|| self.empty_note());
        let overlays = self.picker.is_some()
            || self.help.is_some()
            || self.commands.is_some()
            || self.reviews.is_some()
            || self.posting.is_some()
            || self.menu.is_some();
        self.sync_threads();
        self.doc.now = model::text::now();
        let sending = (self.posting.as_ref())
            .map(|d| (self.sending(d.with_local()), self.reviewing.checked()));
        let (tree_top, tree_rows) = self.tree_rows();
        let picker_top = self.picker_top();
        let (grid, last) = self.screen.frame();
        chrome.outline(grid, &self.theme);
        let last = std::mem::take(&mut self.kept).then_some(last);
        let (visible, scrolled) = self
            .doc
            .draw(grid, last, (x, width, 1), &self.theme, self.frame);
        self.visible = visible;
        // The picker covers the tree, and refreshing it costs a pass over
        // every file while the picker reads a long input.
        if let Some(columns) = tree_columns.filter(|_| self.picker.is_none()) {
            if chrome.overlay {
                // Blank out the diff under it.
                for y in tree_top..tree_top + tree_rows {
                    grid.fill(0, y, columns, Style::PLAIN);
                }
            }
            let place = (tree_top, tree_rows, columns);
            let (focused, joined) = (self.pane == Pane::Tree, framed && !chrome.overlay);
            if let (Some(panel), Some(comments)) = (&mut self.panel, &self.comments) {
                // As the tree follows the file, `m` or a tap in the diff
                // moves the selection.
                if let Some(thread) = self.doc.focused() {
                    panel.select(&thread.root);
                }
                let comments = (comments, self.enumerating);
                let look = (&self.theme, &self.keys);
                panel.draw(grid, place, comments, (focused, joined), look);
            } else {
                let files = self.doc.document.files();
                if self.pane == Pane::Diff && !files.is_empty() {
                    self.tree.follow(self.doc.document.pos.file);
                }
                let totals = Totals::of(files, self.enumerating);
                self.tree.draw(
                    grid,
                    place,
                    files,
                    &totals,
                    self.comments.as_ref(),
                    self.enumerating,
                    focused,
                    joined,
                    &self.theme,
                );
            }
        }
        // After the tree, so an empty one over the diff says why.
        if let Some(text) = empty {
            let left = x + width.saturating_sub(x + text.width()) / 2;
            grid.put_str(left, height / 2, width, &text, self.theme.style(Role::Dim));
        }
        self.switcher = chrome.text(grid, &self.theme);
        self.tree_button = (!overlays)
            .then(|| {
                tree_button(
                    grid,
                    tree_columns,
                    (framed, tree_top + tree_rows),
                    &self.theme,
                )
            })
            .flatten();
        if let Some(line) = &search_line {
            draw_search_line(grid, line, framed, &self.theme);
        }
        if let Some(picker) = &mut self.picker {
            let progress = if !self.enumerating {
                picker::Progress::Done
            } else if self.doc.document.files().len() >= piped::MAX_ENTRIES {
                picker::Progress::Capped
            } else {
                picker::Progress::Reading
            };
            draw_picker(grid, picker, &self.doc, progress, picker_top, &self.theme);
        }
        if let Some(commands) = &mut self.commands {
            commands.draw(grid, &self.theme, &self.keys);
        }
        if let Some(help) = &mut self.help {
            help.draw(grid, &self.theme, &self.keys);
        }
        if let Some(picker) = &mut self.reviews {
            picker.draw(grid, &self.theme);
        }
        if let (Some(dialog), Some(mode), Some(sending)) =
            (&mut self.posting, &self.reviewing.mode, sending)
        {
            dialog.draw(grid, (&self.theme, &self.keys), mode, sending);
        }
        if let Some(menu) = &self.menu {
            menu.draw(grid, &self.theme);
        }
        self.kept = !overlays && !chrome.overlay;
        // The terminal scrolls whole rows, so the cell diff sends the tree's
        // columns again on every row it scrolled: worth it while they're
        // fewer than the diff's cells that moved. Tree cells count a third
        // more, the ratio at which a full tree beside short code lines
        // never costs more than the plain diff; a mostly blank tree would
        // gain on larger scrolls too, which counting the cells that differ
        // would catch.
        Some(self.screen.flush(scrolled.filter(|(rows, by)| {
            3 * (width - x) * (rows.len() - by.unsigned_abs()) > 4 * x * rows.len()
        })))
    }
}

/// `«` on the tree's bottom border (whose row `tree_end` is after), to
/// hide it, or with no tree, `»` at the bottom left, on the frame's border
/// or the bottom row, to show it: for a mouse or a tap. Returns where it
/// went.
fn tree_button(
    grid: &mut term::Grid,
    tree_columns: Option<usize>,
    (framed, tree_end): (bool, usize),
    theme: &Theme,
) -> Option<(usize, usize)> {
    let (width, height) = (grid.width(), grid.height());
    if height < 3 {
        return None;
    }
    let (glyph, at) = match tree_columns {
        Some(columns) if columns >= 4 => ("«", (1, tree_end.checked_sub(1)?)),
        Some(_) => return None,
        None => ("»", (usize::from(framed), height - 1)),
    };
    grid.put_str(at.0, at.1, width, glyph, theme.style(Role::Hint));
    Some(at)
}

/// The search being typed, on the bottom row, inside the frame's corners:
/// `/pattern▏` and a note.
fn draw_search_line(
    grid: &mut term::Grid,
    (typed, note): &(String, String),
    framed: bool,
    theme: &Theme,
) {
    let (width, height) = (grid.width(), grid.height());
    if height < 2 {
        return;
    }
    let (y, inset) = (height - 1, usize::from(framed));
    let end = width.saturating_sub(inset);
    grid.fill(inset, y, end, Style::PLAIN);
    let after = grid.put_str(inset + inset, y, end, typed, theme.style(Role::Hint));
    let after = grid.put_str(after, y, end, "▏", theme.style(Role::Hint));
    grid.put_str(after, y, end, note, theme.style(Role::Dim));
}

/// The picker over every row from `top` but the status row.
fn draw_picker(
    grid: &mut term::Grid,
    picker: &mut Picker,
    doc: &Doc,
    progress: picker::Progress,
    top: usize,
    theme: &Theme,
) {
    let (width, height) = (grid.width(), grid.height());
    let rows_shown = height.saturating_sub(1);
    picker.catch_up(doc.document.files());
    let rows = picker.rows(
        doc.document.files(),
        doc.document.commits(),
        (width, rows_shown),
        progress,
    );
    for y in top..top + rows_shown {
        grid.fill(0, y, width, Style::PLAIN);
    }
    for (i, (row, selected)) in rows.iter().enumerate() {
        if let Some(cells) = row.rows().next() {
            view::paint(grid, 0, top + i, width, cells, theme, Attributes::NONE);
        }
        if *selected {
            grid.map_styles(0, top + i, width, |s| theme.selected(s, true));
        }
    }
}

/// The view modes for the status bar: `split syntax wrap watch`.
fn modes(app: &App) -> String {
    let doc = &app.doc;
    let mut modes = String::new();
    modes.push_str(if doc.layout.split { "split" } else { "unified" });
    if doc.document.structural {
        modes.push_str(" syntax");
    }
    // Only on files it applies to: elsewhere it would be noise.
    let renders = doc
        .document
        .files()
        .get(doc.document.pos.file)
        .is_some_and(|e| {
            matches!(e.source(), Source::Repo) && app.renderers.for_path(&e.change().path).is_some()
        });
    if doc.document.rendered() && renders {
        modes.push_str(" rendered");
    }
    if doc
        .document
        .files()
        .get(doc.document.pos.file)
        .is_some_and(|e| doc.document.is_full(e))
    {
        modes.push_str(" full");
    }
    if app.wrap {
        modes.push_str(" wrap");
    }
    modes.push_str(match app.watch {
        Watch::Off => "",
        Watch::Starting | Watch::Watching => " watch",
        Watch::Polling => " watch:polling",
    });
    if !app.mouse {
        let leader = || {
            let (chord, _) = app
                .keys
                .leader_entries()
                .iter()
                .find(|(_, a)| *a == Act::ToggleMouse)?;
            Some(format!("{} {}", app.key_name(Act::Leader)?, chord.name()))
        };
        let key = app
            .keys
            .chords(false, Act::ToggleMouse)
            .next()
            .map(crate::keys::Chord::name)
            .or_else(leader)
            .map(|k| format!(" ({k})"))
            .unwrap_or_default();
        let _ = write!(modes, " mouse off{key}");
    }
    modes
}

/// What's compared, for the status bar's chip; under the commits of a
/// piped log or several, which commit of how many.
fn chip(app: &App) -> String {
    let doc = &app.doc;
    let growing = if app.enumerating { "+" } else { "" };
    match doc.document.commit_of(doc.document.pos.file) {
        Some(c) if doc.document.commits().len() > 1 || app.piped => {
            format!("commit {}/{}{growing}", c + 1, doc.document.commits().len())
        }
        _ => (app.checkpoint_chip()).unwrap_or_else(|| app.description.clone()),
    }
}

/// Which file of how many, or under a commit, which of its files (on the
/// commit itself, how many it has). A `+` marks counts still growing.
fn position(app: &App) -> Option<String> {
    let doc = &app.doc;
    if doc.document.files().is_empty() {
        return None;
    }
    let growing = if app.enumerating { "+" } else { "" };
    let file = doc.document.pos.file;
    let Some(c) = doc.document.commit_of(file) else {
        let total = doc.document.files().len();
        return Some(format!("{}/{total}{growing}", (file + 1).min(total)));
    };
    let entry = doc.document.commits()[c];
    let files = doc.document.commit_files(entry).len();
    let last = if c + 1 == doc.document.commits().len() {
        growing
    } else {
        ""
    };
    Some(if file == entry {
        let s = if files == 1 { "" } else { "s" };
        format!("{files}{last} file{s}")
    } else {
        format!("{}/{files}{last}", file - entry)
    })
}

/// The narrow switcher's position: the file of how many, under commits
/// the file within its commit, and on a commit the commit of how many.
fn switcher_position(app: &App) -> String {
    let doc = &app.doc;
    let growing = if app.enumerating { "+" } else { "" };
    let file = doc.document.pos.file;
    match doc.document.commit_of(file) {
        Some(c) if doc.document.commits()[c] == file => {
            format!("{}/{}{growing}", c + 1, doc.document.commits().len())
        }
        Some(_) => position(app).unwrap_or_default(),
        None => {
            let total = doc.document.files().len();
            format!("{}/{total}{growing}", (file + 1).min(total))
        }
    }
}

/// The file shown, for the title: on wide screens its state, staging,
/// path and counts (a commit's id and subject), on narrow ones the file
/// switcher.
fn heading(app: &App) -> Option<Heading> {
    let doc = &app.doc;
    let entry = doc.document.files().get(doc.document.pos.file)?;
    let commit = entry.is_commit();
    let counts = entry.counts().filter(|_| !commit && !entry.is_unchanged());
    let staging = entry
        .change()
        .staging
        .filter(|_| {
            !commit && !matches!(entry.change().status, Status::Untracked | Status::Unmerged)
        })
        .map(crate::render::stat::staging_word);
    let path = if entry.is_page() {
        "Conversation".to_owned()
    } else {
        escape(&entry.change().path)
    };
    let (dir, name) = match path.rfind('/') {
        Some(i) => (path[..=i].to_owned(), path[i + 1..].to_owned()),
        None => (String::new(), path.clone()),
    };
    if !app.side_tree() {
        return Some(Heading::Switcher(Switcher {
            position: switcher_position(app),
            name: if commit { path } else { name },
            counts: counts.map(|n| (n.added, n.removed)),
            staging,
        }));
    }
    let dim = |text: &str| (text.to_owned(), Role::Dim);
    if entry.is_page() {
        return Some(Heading::File(Title {
            head: Vec::new(),
            flex: (path, Role::Plain, Cut::Right),
            name: (String::new(), Role::Plain),
            tail: Vec::new(),
        }));
    }
    if let Source::Commit(c) = &entry.source() {
        let (head, subject) = match &c.id {
            Some(id) => (
                vec![
                    ("commit ".to_owned(), Role::Plain),
                    (id[..id.len().min(7)].to_owned(), Role::CommitHeader),
                    dim(" · "),
                ],
                c.subject.clone(),
            ),
            None => (Vec::new(), path),
        };
        return Some(Heading::File(Title {
            head,
            flex: (subject, Role::Plain, Cut::Right),
            name: (String::new(), Role::Plain),
            tail: Vec::new(),
        }));
    }
    let mut head = vec![if entry.is_unchanged() {
        (crate::tui::tree::UNCHANGED.to_owned(), Role::Dim)
    } else {
        (entry.status().word().to_owned(), Role::Plain)
    }];
    if let Some(staging) = staging {
        head.push((format!(" · {staging}"), Role::Plain));
    }
    head.push(dim(" · "));
    let flex = match &entry.change().status {
        Status::Renamed { from, .. } => format!("{} → {dir}", escape(from)),
        _ => dir,
    };
    let tail = counts.map_or_else(Vec::new, |n| {
        vec![
            dim(" · "),
            (format!("+{}", n.added), Role::Added),
            (format!(" -{}", n.removed), Role::Removed),
        ]
    });
    Some(Heading::File(Title {
        head,
        flex: (flex, Role::Dim, Cut::Left),
        name: (name, Role::FileHeader),
        tail,
    }))
}

/// A note for the status bar, in place of the mode words: the question
/// waiting for `y`, the message, or the keys of the text box or of the thread focused or selected in the
/// comments panel, or what its ask is doing, or while lines are selected,
/// how many, or while asks run with nothing else to back out of, that
/// `esc` stops them all.
pub(super) fn note(app: &mut App) -> Option<String> {
    // Looked for only while the focused thread has an ask, not every frame.
    let asked = (app.doc.focused()).is_some_and(|t| app.asking.asking(&t.root));
    let focus_shown = asked && app.doc.focused_shown();
    let app = &*app;
    let asking = app.confirm.as_ref().map(|(prompt, ..)| prompt);
    asking
        .or_else(|| app.message.as_ref().map(|(text, _)| text))
        .cloned()
        .or_else(|| {
            if app.doc.writing().is_some() {
                // A question's keys only, short enough to fit beside the
                // tree on a 100-column screen.
                let asks = match app.asks_whom() {
                    Some(agent) => format!("enter asks {agent}"),
                    None => "enter asks".to_owned(),
                };
                return Some(match (app.on_save, &app.composing) {
                    (OnSave::Ask, Some(super::Compose::Reply(_))) => {
                        format!("{asks} · empty asks now · esc keeps it")
                    }
                    (OnSave::Ask, _) => format!("{asks} · esc keeps it"),
                    _ => "enter saves · alt+enter new line · esc keeps it".to_owned(),
                });
            }
            let panel = app.panel.as_ref().filter(|_| app.pane == Pane::Tree);
            let thread = match panel {
                Some(panel) => panel.thread().filter(|_| !panel.editing)?,
                None => app.doc.focused().filter(|_| app.doc.selection.is_none())?,
            };
            // What its ask is doing, in place of its keys.
            // `esc` stops only this ask where the thread takes the keys: in
            // the diff, on screen. Elsewhere it backs out of something else.
            if let Some(asking) = app.asking.status_of(&thread.root) {
                let takes_esc = panel.is_none() && app.pane == Pane::Diff && focus_shown;
                return Some(if takes_esc {
                    format!("{asking} · esc stops it")
                } else {
                    asking
                });
            }
            let drafts = thread.comments.iter().any(|n| !n.posted());
            let open = !matches!(thread.at, model::At::Conversation { .. });
            let resolve = if thread.resolved { "reopen" } else { "resolve" };
            let reverts = thread.reverts();
            let apply = if reverts { "revert" } else { "apply" };
            // Most used first, as a narrow status bar cuts the end.
            let thread_keys = [
                (Act::Reply, "reply", open),
                (Act::AskAgent, "ask", open),
                (
                    Act::ApplySuggestion,
                    apply,
                    open && thread.suggestion().is_some(),
                ),
                (Act::Resolve, resolve, open),
                (Act::FixWithAgent, "fix", open),
                (Act::EditDraft, "edit", drafts),
                (Act::DeleteDraft, "delete", drafts),
                (Act::OpenOnHost, "open", thread.url.is_some()),
                (Act::Adopt, "make it yours", thread.unposted()),
            ];
            let mut keys: Vec<String> = (thread_keys.into_iter())
                .filter(|&(.., shown)| shown)
                .filter_map(|(action, words, _)| {
                    Some(format!("{} {words}", app.thread_key_name(action)?))
                })
                .collect();
            let move_here = app
                .key_for(false, Act::MoveThread)
                .map(|key| format!("{key} move here"));
            keys.extend(move_here.filter(|_| thread.branch.is_some()));
            Some(keys.join(" · "))
        })
        .or_else(|| {
            let selection = app.doc.selection.as_ref()?;
            let lines = selection.len();
            let s = if lines == 1 { "" } else { "s" };
            let side = match selection.side {
                Side::Old => "old",
                Side::New => "new",
            };
            let copy = app.keys.chords(false, Act::Copy).next()?.name();
            Some(format!("{lines} {side} line{s} · {copy} copies"))
        })
        .or_else(|| {
            // With nothing else to back out of, `esc` stops every ask, not
            // only the one the status bar names.
            let last_layer =
                app.pane == Pane::Diff && app.search.is_none() && app.doc.focused().is_none();
            let cancels =
                app.keys.action(false, term::Key::Esc, term::Mods::NONE) == Some(Act::Cancel);
            (last_layer && cancels && app.asking.busy()).then(|| "esc stops all asks".to_owned())
        })
}

/// The review's part of the status bar: `PR #7 · 18 threads · 2 drafts`,
/// the review in review mode and the counts when the comments file has
/// threads, an ask running before the counts, as `asking claude…
/// thinking…`.
fn review_status(app: &App) -> Option<String> {
    let (threads, drafts) = app.comments.as_ref().map_or((0, 0), Comments::counts);
    let counts = (threads > 0).then(|| {
        let mut text = session::plural(threads, "thread");
        if drafts > 0 {
            let _ = write!(text, " · {}", session::plural(drafts, "draft"));
        }
        text
    });
    let counts = match (app.asking.status_besides(noted_ask(app)), counts) {
        (Some(asking), Some(counts)) => Some(format!("{asking} · {counts}")),
        (asking, counts) => asking.or(counts),
    };
    let review = app.reviewing.status(app.comments.as_ref());
    match (review.map(|words| app.say(&words)), counts) {
        (Some(review), Some(counts)) => Some(format!("{review} · {counts}")),
        (review, counts) => review.or(counts),
    }
}

/// The first comment of the thread whose ask [`note`] shows, so the counts
/// don't show it again: the comments panel's thread, else the focused one,
/// when nothing else has the note.
fn noted_ask(app: &App) -> Option<&str> {
    if app.confirm.is_some() || app.message.is_some() || app.doc.writing().is_some() {
        return None;
    }
    let thread = match app.panel.as_ref().filter(|_| app.pane == Pane::Tree) {
        Some(panel) => panel.thread().filter(|_| !panel.editing)?,
        None => app.doc.focused().filter(|_| app.doc.selection.is_none())?,
    };
    app.asking
        .asking(&thread.root)
        .then_some(thread.root.as_str())
}

/// The status bar's key hints for the focused pane, longest first:
/// `/ search · tab tree · space commands` with the diff focused (`tab
/// comments` while the left panel shows them), `/ filter · tab diff ·
/// space commands` with the tree; then without the pane switch;
/// then the menu alone. Each key is the first one that runs its action
/// where the focus is, and a part whose action has none is left out. On a
/// narrow screen `space` shows as `␣`.
fn hints(app: &App, narrow: bool) -> [Option<String>; 3] {
    let tree = app.pane == Pane::Tree;
    let key = |action| {
        app.keys
            .chords(tree, action)
            .chain(app.keys.chords(false, action))
            .find(|c| app.keys.action(tree, c.key, c.mods) == Some(action))
            .map(crate::keys::Chord::name)
    };
    let (find, other) = if tree && app.panel.is_some() {
        let parts = [
            key(Act::FilterTree).map(|k| format!("{k} search")),
            key(Act::CommentedFiles).map(|k| format!("{k} filter")),
        ];
        let parts: Vec<String> = parts.into_iter().flatten().collect();
        ((!parts.is_empty()).then(|| parts.join(" · ")), "diff")
    } else if tree {
        (key(Act::FilterTree).map(|k| format!("{k} filter")), "diff")
    } else if app.panel.is_some() {
        (key(Act::Search).map(|k| format!("{k} search")), "comments")
    } else {
        (key(Act::Search).map(|k| format!("{k} search")), "tree")
    };
    let switch = key(Act::SwitchPane).map(|k| format!("{k} {other}"));
    let menu = key(Act::Leader).map(|k| {
        let k = if narrow && k == "space" {
            "␣".to_owned()
        } else {
            k
        };
        format!("{k} commands")
    });
    let join = |parts: &[&Option<String>]| {
        let parts: Vec<&str> = parts.iter().filter_map(|p| p.as_deref()).collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    };
    [join(&[&find, &switch, &menu]), join(&[&find, &menu]), menu]
}
