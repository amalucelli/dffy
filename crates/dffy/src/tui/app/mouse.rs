//! The mouse: clicks, drags and shift-clicks on the diff and the tree, and
//! the menu a right click opens, whose entries run what their keys do.

use std::time::Instant;

use dffy_core::document::{Pos, Side};

use super::{Action, App, MULTI_CLICK, Pane, Press, WHEEL_ROWS};
use crate::keys::{Action as Act, Table};
use crate::review::model;
use crate::term::Input;
use crate::tui::menu::{self, Menu, Offer, Run};
use crate::tui::tree::Item;
use crate::tui::view::{Block, Doc, Hit};

impl App {
    /// Input other than a key with no overlay open: the mouse.
    pub(super) fn pointer(&mut self, input: &Input) -> Option<Action> {
        match *input {
            Input::Scroll { delta, x, .. } => {
                let rows = isize::from(delta) * WHEEL_ROWS;
                if !self.over_tree(usize::from(x)) {
                    self.scroll_diff(rows);
                } else if let Some(panel) = &mut self.panel {
                    if let Some(root) = panel.scroll_by(isize::from(delta)) {
                        self.follow_thread(&root);
                    }
                } else {
                    self.tree.scroll_by(rows);
                }
            }
            Input::Click { x, y } => self.click(usize::from(x), usize::from(y)),
            Input::ShiftClick { x, y } => self.shift_click(usize::from(x), usize::from(y)),
            Input::PeekClick { x, y } => self.peek_click(usize::from(x), usize::from(y)),
            Input::Drag { x, y } => self.drag(usize::from(x), usize::from(y)),
            Input::Release { .. } => return self.release(),
            Input::RightClick { x, y } => self.open_menu(usize::from(x), usize::from(y)),
            Input::Key(..) | Input::Background { .. } | Input::Paste(_) => {}
        }
        None
    }

    /// A right click elsewhere opens the menu there instead.
    pub(super) fn menu_input(&mut self, input: &Input) -> Option<Action> {
        // An armed `d` is kept for the key right after it, never a later one.
        self.deleting = false;
        let size = (self.screen.width(), self.screen.height());
        let menu = self.menu.as_mut()?;
        let outcome = match *input {
            Input::Key(key, mods) => menu.key(key, mods),
            Input::Click { x, y } => menu.tap((usize::from(x), usize::from(y)), size),
            Input::RightClick { x, y } => {
                self.menu = None;
                self.open_menu(usize::from(x), usize::from(y));
                return None;
            }
            _ => menu::Outcome::Stay,
        };
        match outcome {
            menu::Outcome::Stay => None,
            menu::Outcome::Close => {
                self.menu = None;
                None
            }
            menu::Outcome::Run(run) => {
                self.menu = None;
                match run {
                    Run::Action(action) => self.dispatch(action),
                    Run::CopyLine => {
                        self.dispatch(Act::Select);
                        self.dispatch(Act::Copy)
                    }
                    Run::Thread(action) => self.thread_act(action, false).ok().flatten(),
                    Run::Reveal(separator) => {
                        self.doc.aim(separator);
                        self.dispatch(Act::Expand)
                    }
                    Run::Peek(action) => self.peek_act(action),
                }
            }
        }
    }

    /// A right click: aims at what's under it as a left click does, without
    /// opening, folding or revealing anything, then lists what the keys do
    /// to it there. With nothing to act on, no menu opens.
    fn open_menu(&mut self, x: usize, y: usize) {
        self.press = None;
        self.drag_edge = None;
        self.deleting = false;
        let (tree_top, tree_rows) = self.tree_rows();
        let entries = if self.over_tree(x) && (tree_top..tree_top + tree_rows).contains(&y) {
            // Under the panel's top border.
            y.checked_sub(tree_top + 1)
                .map(|line| self.aim_tree(line))
                .unwrap_or_default()
        } else if y == 0 || y + 1 >= self.screen.height() || x + 1 == self.screen.width() {
            Vec::new()
        } else {
            self.aim_diff(x, y)
        };
        if !entries.is_empty() {
            self.menu = Some(Menu::new((x, y), entries));
        }
    }

    /// Selects the tree's row at `line`, the keys on the tree; for a file,
    /// shows it, as moving onto it does, and lists the tree's keys for it.
    fn aim_tree(&mut self, line: usize) -> Vec<menu::Entry> {
        if let Some(panel) = &mut self.panel {
            let Some(root) = panel.click(line) else {
                return Vec::new();
            };
            self.pane = Pane::Tree;
            // Copy comment takes the first: no comment of the row was tapped.
            self.tapped_comment = None;
            self.follow_thread(&root);
            let Some(thread) = self.target_thread() else {
                return Vec::new();
            };
            let shown = self.doc.focused().is_some_and(|t| t.root == root);
            return self.menu_entries(self.thread_offers(&thread, shown));
        }
        let Some(item) = self.tree.click(line) else {
            return Vec::new();
        };
        self.pane = Pane::Tree;
        // A file outside the diff, in the all-files tree: what enter and
        // `x` do to it.
        if let Item::Path(i) = item {
            let path = self.tree.all_path(i);
            let opened = path.is_some_and(|p| self.unchanged_file(p.as_ref()).is_some());
            let mut entries = vec![self.menu_entry(true, "Open whole", Act::Open)];
            if opened {
                entries.push(self.menu_entry(true, "Close file", Act::CloseFile));
            }
            entries.push(self.menu_entry(true, "Diff's files only", Act::ToggleAllFiles));
            return entries;
        }
        let Item::File(file) = item else {
            return Vec::new();
        };
        // `e` would open a line tapped before, maybe in another file.
        self.tapped = None;
        self.browse(file);
        let entry = &self.doc.document.files()[file];
        let on_disk = !entry.is_commit() && !self.piped;
        let editable = on_disk && entry.change().new.is_some();
        let mut entries = vec![self.menu_entry(true, "Copy path", Act::CopyPath)];
        if on_disk {
            entries.push(self.menu_entry(true, "Copy absolute path", Act::CopyAbsolutePath));
        }
        if editable {
            entries.push(self.menu_entry(true, "Open in editor", Act::Edit));
        }
        if self.applies(Act::OpenTerminal) {
            entries.push(self.menu_entry(true, "Open terminal here", Act::OpenTerminal));
        }
        let mark = if entry.viewed() {
            "Mark unviewed"
        } else {
            "Mark viewed"
        };
        entries.push(self.menu_entry(true, mark, Act::MarkViewed));
        if self.comments.is_some() {
            if on_disk {
                entries.push(self.menu_entry(true, "Comment on file", Act::CommentFile));
            }
            let filter = if self.tree.commented() {
                "Show every file"
            } else {
                "Show only files with comments"
            };
            entries.push(self.menu_entry(true, filter, Act::CommentedFiles));
        }
        entries
    }

    /// Aims at the diff's screen cell `x`, `y`: a thread takes the focus, a
    /// separator is the one `x` reveals, and a line of code is where `v`
    /// starts, the selection kept only when it holds the line. Lists the
    /// keys for it.
    fn aim_diff(&mut self, x: usize, y: usize) -> Vec<menu::Entry> {
        self.pane = Pane::Diff;
        self.doc.comment_selection = None;
        match self.doc.block_row_at(y - 1) {
            Some((Block::Thread(i), _)) => {
                let thread = self.doc.threads()[i].clone();
                self.doc.selection = None;
                self.doc.set_focus(Some(thread.root.clone()));
                self.tapped_comment = self.doc.comment_at(y - 1);
                return self.menu_entries(self.thread_offers(&thread, true));
            }
            Some((Block::New, _)) => return Vec::new(),
            Some((Block::Peek, _)) => {
                self.doc.selection = None;
                self.peek_tap(x, y);
                return self.peek_menu();
            }
            None => self.doc.set_focus(None),
        }
        if let Some(separator) = self.doc.separator_at(y - 1) {
            self.doc.selection = None;
            return vec![menu::Entry {
                run: Run::Reveal(separator),
                ..self.menu_entry(false, "Reveal hidden lines", Act::Expand)
            }];
        }
        let Some((pos, side)) = self.code_at(x, y) else {
            self.doc.selection = None;
            return Vec::new();
        };
        let selected = self.doc.selection_holds(pos, side);
        if !selected {
            self.doc.selection = None;
        }
        self.tapped = Some((pos, side));
        let entry = &self.doc.document.files()[pos.file];
        let editable = !entry.is_commit() && !self.piped && entry.change().new.is_some();
        let mut entries = self.menu_entries(self.line_offers(pos.file, selected));
        if !selected {
            let key = match (
                self.key_for(false, Act::Select),
                self.key_for(false, Act::Copy),
            ) {
                (Some(v), Some(y)) => format!("{v} {y}"),
                _ => String::new(),
            };
            entries.push(menu::Entry {
                label: "Copy line".to_owned(),
                key,
                run: Run::CopyLine,
            });
        }
        let mut offers = self.edit_offers(pos.file, selected);
        offers.extend(self.conflict_offers(Some(pos)));
        entries.extend(self.menu_entries(offers));
        if !selected && self.navigation_offered(pos.file, side) {
            entries.push(self.menu_entry(false, "Go to definition", Act::GoToDefinition));
        }
        if editable {
            entries.push(self.menu_entry(false, "Open in editor", Act::Edit));
        }
        if self.applies(Act::OpenTerminal) {
            entries.push(self.menu_entry(false, "Open terminal here", Act::OpenTerminal));
        }
        entries
    }

    /// What the keys do to the lines selected in `file`, or to the line
    /// aimed at: comment and suggest where comments can go, ask and fix on
    /// a selection there, and copy a selection.
    pub(super) fn line_offers(&self, file: usize, selected: bool) -> Vec<Offer> {
        let main = |label: &str, action| Offer::new(label, action, Table::Main);
        let commentable = self.comments.is_some()
            && (self.doc.document.files().get(file)).is_some_and(|e| !e.is_commit());
        let mut offers = Vec::new();
        if commentable {
            let label = if selected {
                "Comment on selection"
            } else {
                "Comment on line"
            };
            offers.push(main(label, Act::Comment));
            offers.push(main("Suggest change", Act::SuggestChange));
            if selected {
                offers.push(main(&self.ask_label(), Act::AskAgent));
                offers.push(main("Fix with agent", Act::FixWithAgent));
            }
        }
        if selected {
            offers.push(main("Copy selection", Act::Copy));
            offers.push(main("Copy as prompt", Act::CopyPrompt));
        }
        offers
    }

    /// The thread keys, as far as they'd act on `thread`, then its copies;
    /// folding only when the diff shows it (`folds`).
    pub(super) fn thread_offers(&self, thread: &model::Thread, folds: bool) -> Vec<Offer> {
        let key = |label: &str, action| Offer::new(label, action, Table::Thread);
        let main = |label: &str, action| Offer::new(label, action, Table::Main);
        let copies = [
            main("Copy comment", Act::CopyComment),
            main("Copy thread", Act::CopyThread),
            key("Copy as prompt", Act::CopyPrompt),
        ];
        // The conversation is read-only, and never folds.
        if matches!(thread.at, model::At::Conversation { .. }) {
            return (thread.url.as_deref().is_some_and(model::text::is_web))
                .then(|| key("Open on host", Act::OpenOnHost))
                .into_iter()
                .chain(copies)
                .collect();
        }
        let mut offers = vec![
            key("Reply", Act::Reply),
            key(&self.ask_label(), Act::AskAgent),
        ];
        if thread.reverts() {
            offers.push(key("Revert suggestion", Act::ApplySuggestion));
        } else if thread.suggestion().is_some() {
            offers.push(key("Apply suggestion", Act::ApplySuggestion));
        }
        offers.push(key("Fix with agent", Act::FixWithAgent));
        if thread.comments.iter().any(|n| !n.posted()) {
            offers.push(key("Edit draft", Act::EditDraft));
            offers.push(key("Delete draft", Act::DeleteDraft));
        }
        if thread.unposted() {
            offers.push(key("Make it yours", Act::Adopt));
        }
        if thread.branch.is_some() {
            offers.push(main("Move to this branch", Act::MoveThread));
        }
        offers.push(key(
            if thread.resolved { "Reopen" } else { "Resolve" },
            Act::Resolve,
        ));
        if folds {
            let fold = if self.doc.fold().collapsed(&thread.root) {
                "Unfold"
            } else {
                "Fold"
            };
            offers.push(main(fold, Act::ToggleThread));
        }
        if thread.url.as_deref().is_some_and(model::text::is_web) {
            offers.push(key("Open on host", Act::OpenOnHost));
        }
        offers.extend(copies);
        offers
    }

    /// The ask's entry: `Ask claude` once the agent plugin said which
    /// agent answers, which it says only once asked.
    fn ask_label(&self) -> String {
        format!("Ask {}", self.asking.default_agent().unwrap_or("agent"))
    }

    /// `offers` as the menu's entries, each with the key that does it
    /// where it's aimed, running as that key does.
    fn menu_entries(&self, offers: Vec<Offer>) -> Vec<menu::Entry> {
        (offers.into_iter())
            .map(|o| match o.table {
                Table::Thread => menu::Entry {
                    key: self.thread_key_name(o.action).unwrap_or_default(),
                    label: o.label,
                    run: Run::Thread(o.action),
                },
                Table::Conflict => menu::Entry {
                    key: self.conflict_key(o.action),
                    ..self.menu_entry(false, &o.label, o.action)
                },
                table => self.menu_entry(table == Table::Tree, &o.label, o.action),
            })
            .collect()
    }

    /// An entry running `action` as its key does, in the tree (`tree`) or
    /// the diff; with no key there, as the command panel's does, shown as
    /// its two keys: `␣ "`.
    pub(super) fn menu_entry(&self, tree: bool, label: &str, action: Act) -> menu::Entry {
        let leader = || {
            let (chord, _) = (self.keys.leader_entries().iter()).find(|(_, a)| *a == action)?;
            let leader = self.key_name(Act::Leader)?;
            let leader = if leader == "space" {
                "␣".to_owned()
            } else {
                leader
            };
            Some(format!("{leader} {}", chord.name()))
        };
        menu::Entry {
            label: label.to_owned(),
            key: self
                .key_for(tree, action)
                .or_else(leader)
                .unwrap_or_default(),
            run: Run::Action(action),
        }
    }

    /// The tree's columns, beside the diff or over it.
    pub(super) fn tree_columns(&self) -> Option<usize> {
        self.tree_shown
            .then(|| self.side_width().unwrap_or_else(|| self.screen.width()))
    }

    pub(super) fn over_tree(&self, x: usize) -> bool {
        self.tree_columns().is_some_and(|w| x < w)
    }

    /// The code row at screen cell `x`, `y` and the side it's on there.
    pub(super) fn code_at(&mut self, x: usize, y: usize) -> Option<(Pos, Side)> {
        let pos = self.doc.row_at(y.checked_sub(1)?)?;
        let side = self.doc.side_at(pos, x.checked_sub(self.content_x())?)?;
        Some((pos, side))
    }

    /// A tap on the diff's cell `x`, `y` (the `count`th in a row) off a
    /// box: a separator reveals, and in file view a rule, or an edited
    /// line's gutter, opens or folds its change. What the cell shows, for a
    /// press to go on from, unless a change opened or folded there.
    fn tap_code(&mut self, x: usize, y: usize, count: u8) -> Option<Hit> {
        // The rows under the tap changed: there's nothing more to hit.
        if let Some(toggled) = self.doc.click(y - 1, count == 1) {
            self.say_run(Some(toggled));
            return None;
        }
        let hit = (x.checked_sub(self.content_x()))
            .and_then(|column| self.doc.hit(y - 1, column, None))?;
        if let Hit::Gutter { pos, .. } = hit
            && count == 1
            && let Some(toggled) = self.doc.gutter_click(pos)
        {
            self.say_run(Some(toggled));
            return None;
        }
        Some(hit)
    }

    fn click(&mut self, x: usize, y: usize) {
        self.press = None;
        self.drag_edge = None;
        let count = match self.clicks {
            Some((at, cell, n)) if cell == (x, y) && at.elapsed() <= MULTI_CLICK => n % 3 + 1,
            _ => 1,
        };
        self.clicks = Some((Instant::now(), (x, y), count));
        if self.tree_button == Some((x, y)) {
            self.toggle_tree();
            return;
        }
        let (tree_top, tree_rows) = self.tree_rows();
        let on_tree = self.over_tree(x) && (tree_top..tree_top + tree_rows).contains(&y);
        if !on_tree && (y == 0 || y + 1 >= self.screen.height()) {
            match &self.switcher {
                Some((row, [prev, ..])) if *row == y && prev.contains(&x) => self.doc.prev_file(),
                Some((row, [_, name, _])) if *row == y && name.contains(&x) => self.open_picker(),
                Some((row, [.., next])) if *row == y && next.contains(&x) => self.doc.next_file(),
                _ => {}
            }
            return;
        }
        self.doc.selection = None;
        self.doc.comment_selection = None;
        if self.over_tree(x) {
            self.pane = Pane::Tree;
            // Under the panel's top border.
            let Some(line) = y.checked_sub(tree_top + 1) else {
                return;
            };
            if let Some(panel) = &mut self.panel {
                if let Some(root) = panel.click(line) {
                    self.open_thread(&root);
                }
                return;
            }
            match self.tree.click(line) {
                Some(Item::File(file)) => self.open(file),
                Some(Item::Dir { open }) => self.tree.set_open(!open),
                // A tap shows the commit's header and its files.
                Some(Item::Commit { file, open }) => {
                    if !open {
                        self.tree.set_open(true);
                    }
                    self.open(file);
                }
                // Opened as the tap ends (see `App::release`).
                Some(Item::Path(i)) => self.tree_tap = Some(i),
                None => {}
            }
            return;
        }
        self.pane = Pane::Diff;
        if x + 1 == self.screen.width() {
            self.doc.overview_click(y - 1);
            return;
        }
        // A tap on a thread focuses it, and on its header also folds or
        // unfolds it; one on code leaves it. A press on a comment's text
        // starts a selection there as one on code does.
        match self.doc.block_row_at(y - 1) {
            Some((Block::Thread(i), header)) => {
                let root = self.doc.threads()[i].root.clone();
                if header {
                    self.doc.toggle_fold(&root);
                }
                self.doc.set_focus(Some(root));
                self.tapped_comment = self.doc.comment_at(y - 1);
                let hit = x
                    .checked_sub(self.content_x())
                    .filter(|_| !header)
                    .and_then(|column| self.doc.comment_hit(y - 1, column, None));
                if let Some(hit) = hit {
                    if count > 1 {
                        self.doc.select_word(&hit, count == 3);
                    }
                    self.press = Some(Press {
                        hit,
                        at: (x, y),
                        selected: count > 1,
                    });
                }
                return;
            }
            Some((Block::New, _)) => return,
            Some((Block::Peek, _)) => {
                self.peek_tap(x, y);
                return;
            }
            None => {
                self.doc.set_focus(None);
                self.blur_peek();
            }
        }
        self.tapped = self.code_at(x, y);
        let Some(hit) = self.tap_code(x, y, count) else {
            return;
        };
        // A double click selects the word, a triple click the line.
        if count > 1 && matches!(hit, Hit::Code { .. }) {
            self.doc.select_word(&hit, count == 3);
        }
        self.press = Some(Press {
            hit,
            at: (x, y),
            selected: count > 1,
        });
    }

    /// The pointer moved with the button held: a selection from where it
    /// went down to the cell under it, by characters from code or in the
    /// comment it started in, by lines from the line numbers, and on the
    /// side it started on. Held past the top or bottom of the pane, the
    /// view scrolls and the selection goes on, a line at a time.
    pub(super) fn drag(&mut self, x: usize, y: usize) {
        let Some(press) = &mut self.press else {
            return;
        };
        if !press.selected {
            if (x, y) == press.at {
                return;
            }
            press.selected = true;
            self.doc.select_hit(&press.hit);
        }
        if let Hit::Comment {
            thread, comment, ..
        } = press.hit
        {
            let pane = y.saturating_sub(1);
            self.drag_edge = None;
            if y == 0 || pane >= self.doc.height {
                self.drag_edge = Some((x, y));
                self.doc.scroll(if y == 0 { -1 } else { 1 });
            }
            let column = x.saturating_sub(self.content_x());
            let row = pane.min(self.doc.height.saturating_sub(1));
            if let Some(hit) = self.doc.comment_hit(row, column, Some((thread, comment))) {
                self.doc.select_to_hit(&hit);
            }
            return;
        }
        let Some(side) = self.doc.selection.as_ref().map(|s| s.side) else {
            return;
        };
        let pane = y.saturating_sub(1);
        if y == 0 || pane >= self.doc.height {
            self.drag_edge = Some((x, y));
            self.doc.select_by(if y == 0 { -1 } else { 1 });
            return;
        }
        self.drag_edge = None;
        let column = x.saturating_sub(self.content_x());
        if let Some(hit) = self.doc.hit(pane, column, Some(side)) {
            self.doc.select_to_hit(&hit);
        }
    }

    /// A press with shift held: the selection, of code or in a comment,
    /// stretches to the cell as a drag there would; with none, one starts
    /// at what the last click was on. Anywhere else it's a click.
    fn shift_click(&mut self, x: usize, y: usize) {
        let column = x.checked_sub(self.content_x());
        let pane = y.checked_sub(1).filter(|&row| row < self.doc.height);
        let (Some(column), Some(pane)) = (column, pane) else {
            return self.click(x, y);
        };
        if self.over_tree(x) {
            return self.click(x, y);
        }
        // A press on another file, or on a row scrolled away, isn't one to
        // select from.
        let on_screen = |doc: &Doc, hit: &Hit| match hit {
            Hit::Code { pos, .. } | Hit::Gutter { pos, .. } => {
                (0..doc.height).any(|y| doc.row_at(y) == Some(*pos))
            }
            Hit::Comment { .. } => true,
        };
        if self.anchor.as_ref().is_some_and(|(file, hit)| {
            *file != self.doc.document.pos.file || !on_screen(&self.doc, hit)
        }) {
            self.anchor = None;
        }
        if self.doc.selection.is_none()
            && self.doc.comment_selection.is_none()
            && let Some((_, anchor)) = &self.anchor
        {
            self.doc.select_hit(anchor);
        }
        let hit = match (
            self.anchor.as_ref().map(|(_, hit)| hit),
            &self.doc.selection,
        ) {
            (
                Some(Hit::Comment {
                    thread, comment, ..
                }),
                _,
            ) if self.doc.comment_selection.is_some() => {
                self.doc
                    .comment_hit(pane, column, Some((*thread, *comment)))
            }
            (_, Some(selection)) => {
                let side = selection.side;
                self.doc.hit(pane, column, Some(side))
            }
            _ => None,
        };
        let Some(hit) = hit else {
            return self.click(x, y);
        };
        self.doc.select_to_hit(&hit);
        self.press = Some(Press {
            hit,
            at: (x, y),
            selected: true,
        });
    }

    /// The button let go: what a drag or a double or triple click selected
    /// goes on the clipboard, and stays selected for `y` to copy again.
    fn release(&mut self) -> Option<Action> {
        self.drag_edge = None;
        // A file outside the diff tapped in the all-files tree opens as the
        // tap ends, the loop looking it up.
        if let Some(i) = self.tree_tap.take() {
            return self.open_all_path(i);
        }
        let press = self.press.take()?;
        self.anchor = Some((self.doc.document.pos.file, press.hit.clone()));
        if press.selected && (self.doc.selection.is_some() || self.doc.comment_selection.is_some())
        {
            return self.copy();
        }
        None
    }
}
