//! Key bindings for the viewer: which key runs which named action. The
//! defaults are here; `keybind = <key>=<action>` lines in the config file
//! change them, Ghostty-style, and `<key>=unbind` frees a key.
//!
//! Keys pressed while the file tree has focus look in the tree's table
//! first (`keybind = tree:<key>=<action>`), then in the main one, so `v`
//! marks a file viewed in the tree and selects lines in the diff. The same
//! goes for the thread's table (`thread:`) while a comment thread has the
//! keys, so `r` replies there and toggles the rendered view elsewhere, and
//! for the conflict's table (`conflict:`) while the file at the cursor has
//! conflicts to resolve, so `o` keeps the current side there and finds a
//! file elsewhere. A thread's keys come before a conflict's, and a
//! go-to-definition peek's (`peek:`) before both while it has the keys, so
//! `d` shows the definition in the diff there. The command
//! panel has a table of its own (`leader:`), whose keys mean what they mean
//! in the diff wherever the diff binds them too.

use crate::config::Invalid;
use crate::render::Canvas;
use crate::term::{Key, Mods};
use crate::theme::Role;

macro_rules! actions {
    ($($variant:ident $group:ident $name:literal $short:literal $doc:literal,)*) => {
        /// Everything a key can do, in the order the help lists them.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum Action {
            $($variant,)*
        }

        impl Action {
            pub(crate) const ALL: &[Self] = &[$(Self::$variant,)*];

            /// The action's name in `keybind` lines.
            pub(crate) fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// A few words for the `?` overlay, where [`Action::doc`] is too
            /// long to scan; the generated docs keep the description.
            pub(crate) fn short(self) -> &'static str {
                match self {
                    $(Self::$variant => $short,)*
                }
            }

            pub(crate) fn doc(self) -> &'static str {
                match self {
                    $(Self::$variant => $doc,)*
                }
            }

            /// The group the help lists it in.
            pub(crate) fn group(self) -> Group {
                match self {
                    $(Self::$variant => Group::$group,)*
                }
            }
        }
    };
}

actions! {
    Leader General "leader" "command panel" "open the command panel; its next key runs a command",
    ScrollDown Navigation "scroll_down" "scroll down" "scroll down a line; in the tree, move down; in a peek, the next definition",
    ScrollUp Navigation "scroll_up" "scroll up" "scroll up a line; in the tree, move up; in a peek, the definition before",
    PageDown Navigation "page_down" "page down" "scroll down a screen",
    PageUp Navigation "page_up" "page up" "scroll up a screen",
    HalfPageDown Navigation "half_page_down" "half page down" "scroll down half a screen",
    HalfPageUp Navigation "half_page_up" "half page up" "scroll up half a screen",
    Top Navigation "top" "top of file" "top of the file",
    Bottom Navigation "bottom" "bottom of file" "bottom of the file",
    NextChange Navigation "next_change" "next change" "next change, on into the next file",
    PrevChange Navigation "prev_change" "prev change" "previous change, back into the previous file",
    NextFile Files "next_file" "next file" "next file",
    PrevFile Files "prev_file" "prev file" "previous file",
    Expand Layout "expand" "more context" "show more of the highlighted ⋯ gap",
    ToggleFull Layout "toggle_full" "toggle whole file" "whole files or changes only, for every file",
    ToggleFileView Layout "toggle_file_view" "toggle file view" "file view, or the diff again: every file whole as it is in one version, the newer first, with a mark in the gutter where the other version's lines were; s then flips the version",
    ScrollRight Navigation "scroll_right" "scroll right" "scroll the code right, without wrapping",
    ScrollLeft Navigation "scroll_left" "scroll left" "scroll the code left",
    ScrollReset Navigation "scroll_reset" "scroll to line start" "scroll the code back to its first column",
    Select Copy "select" "select lines" "select lines, from the top line or the one tapped",
    Copy Copy "copy" "copy selection" "copy the selected lines' code",
    CopyPath Copy "copy_path" "copy path" "copy the file's path",
    CopyAbsolutePath Copy "copy_absolute_path" "copy absolute path" "copy where the file is on disk, from the filesystem's root",
    CopyComment Comments "copy_comment" "copy comment" "copy a comment of the focused thread as written, in Markdown: the one last clicked, else its first",
    CopyThread Comments "copy_thread" "copy thread" "copy the focused thread: each comment under its author and date, as written",
    CopyPrompt Copy "copy_prompt" "copy as prompt" "copy the focused thread, else the selected lines, as a prompt for a coding agent: the comments, the lines with those around them and the file's changes near them",
    Cancel Copy "cancel" "cancel selection" "end the selection; in the tree, clear the filter",
    SwitchPane Files "switch_pane" "switch panel/diff" "switch between the left panel (the file tree, or the comments) and the diff",
    ToggleTree Files "toggle_tree" "toggle tree" "show or hide the file tree",
    OpenPicker Files "open_picker" "find file" "find a file: type part of its path, enter opens it",
    Search Files "search" "search diff" "search the diff's text; n/N then go between matches, esc clears",
    FilterTree Files "filter_tree" "filter tree" "filter the file tree, fuzzily; f/F and n/N keep to the files it keeps, esc clears",
    Open Files "open" "open file or folder" "open a file, or open or close a folder, in the tree; in a peek, the definition's file at its line: its entry in the diff, else the whole file",
    Collapse Files "collapse" "collapse folder" "collapse a folder in the tree",
    ParentFolder Files "parent_folder" "parent folder" "go to the folder holding the file, in the tree",
    GoToDefinition Navigation "go_to_definition" "go to definition" "list the names on the line last tapped, else the top line; type to narrow them, and enter shows where the one picked is defined, in the line's version, in a peek under the line. ctrl+click or alt+click on a name peeks it at once",
    ShowInDiff Peek "show_in_diff" "show in diff" "in a peek: the definition's lines in the diff, where the diff changes its file",
    OtherVersion Peek "other_version" "other version" "in a peek: the name's definitions in the diff's other version",
    References Peek "references" "definitions/uses" "in a peek: jump between the name's definitions and the lines using it, which the peek lists below them as they're found, closest files first",
    MoreUses Peek "more_uses" "more uses" "in a peek: go on past the first 200 files using the name, counting the rest",
    CallsOnly Peek "calls_only" "calls only" "in a peek: list only the uses calling the name, or every use again",
    UsesInDiff Peek "uses_in_diff" "uses in diff" "in a peek: list only the uses in files the diff changes, or those in every file again",
    ClosePeek Peek "close_peek" "close peek" "close the peek",
    JumpBack Navigation "jump_back" "jump back" "back to where the last jump left, with the line that was at the top at the top again: a jump is a definition opened from a peek, or a file opened from find file, the tree or the control API. The last 100 places are kept; one whose file isn't shown any more is skipped",
    JumpForward Navigation "jump_forward" "jump forward" "forward again, to where jump back left",
    MarkViewed Files "mark_viewed" "mark viewed" "mark a file in the tree viewed",
    CloseFile Files "close_file" "close file" "close a file opened outside the diff (from find file), the one selected in the tree or else the one shown; one with comments stays",
    ToggleAllFiles Layout "toggle_all_files" "toggle all files" "the tree lists every file of the repository, the diff's changes colored among them, or the diff's files again; s in the tree then shows the other version. Folders open as you go, enter on a file outside the diff shows it whole",
    TreeWider Files "tree_wider" "widen tree" "widen the tree",
    TreeNarrower Files "tree_narrower" "narrow tree" "narrow the tree",
    CycleLayout Layout "cycle_layout" "cycle layout" "layout: split, unified, then auto (split on wide screens); while selecting, the side copied; in file view, the side read",
    ToggleWrap Layout "toggle_wrap" "toggle wrap" "wrap or truncate long lines",
    ToggleSyntax Diff "toggle_syntax" "toggle syntax diff" "diff by lines or by syntax",
    ToggleRendered Diff "toggle_rendered" "toggle rendered view" "files with a renderer plugin rendered, or as lines (experimental, see render)",
    CycleIndicators Display "cycle_indicators" "cycle indicators" "how changed lines are marked: background, bars, classic signs or none",
    CycleModifiedLines Display "cycle_modified_lines" "cycle modified lines" "a line and its edit in the modified color with red and green tokens, or red and green lines",
    ToggleHighlight Display "toggle_highlight" "toggle highlighting" "syntax highlighting on or off",
    ToggleWhitespace Diff "toggle_whitespace" "ignore whitespace" "ignore whitespace (-w) or not, diffing again",
    ToggleLineNumbers Display "toggle_line_numbers" "toggle line numbers" "show or hide line numbers",
    ToggleHunkHeaders Display "toggle_hunk_headers" "toggle hunk headers" "show or hide the @@ numbers of hunks",
    ToggleStatusBar Layout "toggle_status_bar" "move status bar" "the status bar in the frame's bottom border or its top one",
    CycleTheme Display "cycle_theme" "next theme" "the next theme, built-in or yours",
    ToggleWatch Modes "toggle_watch" "toggle watch" "watch for changes",
    TakeCheckpoint Modes "take_checkpoint" "take checkpoint" "take a checkpoint of the working tree: its files as plain dffy lists them, staged or not and untracked, kept under a local ref of this worktree, the last 5 of them; starting Fix with agent takes one too",
    SinceCheckpoint Modes "since_checkpoint" "since checkpoint" "show only what changed since the latest checkpoint, such as an agent's turn since Fix with agent started it, or the diff shown before again; the status bar says how long ago it was taken",
    ToggleMouse Copy "toggle_mouse" "toggle mouse" "take mouse input, or leave it to the terminal's own selection (many terminals select natively with shift held while dragging)",
    ToggleCopyDecorations Copy "toggle_copy_decorations" "copy decorations" "copies include line numbers and +/- signs, or only the code",
    Stage Changes "stage" "stage lines" "stage the selected lines, else the hunk of the line last tapped or the first on screen: their working-tree version goes in the index (uncommitted changes only)",
    Unstage Changes "unstage" "unstage lines" "unstage the selected lines, else the hunk of the line last tapped or the first on screen: their HEAD version goes back in the index (uncommitted changes only)",
    Discard Changes "discard" "discard lines" "discard the selected lines' unstaged changes, else the hunk's, after a y/n: the working tree gets back the index's version, and an untracked file is deleted (uncommitted changes only)",
    NextConflict Changes "next_conflict" "next conflict" "in a conflicted file, the next conflict",
    PrevConflict Changes "prev_conflict" "prev conflict" "in a conflicted file, the previous conflict",
    KeepCurrent Changes "keep_current" "keep current" "resolve the conflict last gone to, else the first on screen, to the current side (ours); again takes the choice back. Choices show as a preview until written",
    TakeIncoming Changes "take_incoming" "take incoming" "resolve the conflict to the incoming side (theirs); again takes the choice back",
    KeepBoth Changes "keep_both" "keep both" "resolve the conflict to both sides, the current one first; again takes the choice back",
    TakeBase Changes "take_base" "take base" "resolve the conflict to the common ancestor's lines, when git wrote them (merge.conflictStyle diff3 or zdiff3); again takes the choice back",
    WriteResolution Changes "write_resolution" "write resolved" "write the conflicted file as the choices resolve it, asking first when some conflicts have none; then git add it after a y/n",
    Edit General "edit" "open in editor" "open the file in $EDITOR at the line last tapped, else the top line",
    OpenTerminal Agents "open_terminal" "open terminal here" "open a new tab in your terminal, or a window where that's all it offers, at the working tree's root; where dffy can't, it copies a cd there instead",
    Comment Comments "comment" "comment on lines" "comment on the selected lines, or the line v would start on, in a text box under them; enter sends it",
    SuggestChange Comments "suggest_change" "suggest change" "suggest a change to the selected lines, or the line v would start on: a comment's text box holding them, to edit into what they should be; enter sends it",
    CommentFile Comments "comment_file" "comment on file" "comment on the whole file shown, in a text box under its first row; enter sends it",
    Comments Comments "comments" "comments panel" "the left panel lists every comment thread in place of the files, or the files again; moving through the threads shows each in the diff, enter or tab goes to it; the thread keys act on the selected thread, and c filters them by state",
    NextThread Navigation "next_thread" "next thread" "next comment thread, on into the next file with comments; it takes the focus, where r replies, e edits, x resolves, d deletes and o opens it on its host",
    PrevThread Navigation "prev_thread" "prev thread" "previous comment thread, back into the previous file with comments",
    ToggleThread Layout "toggle_thread" "fold thread" "fold or unfold the focused comment thread, or the first on screen",
    ToggleThreads Layout "toggle_threads" "fold all threads" "fold every comment thread to one row, or unfold them all",
    MoveThread Comments "move_thread" "move thread here" "move the focused thread, or the comments panel's selected one, from the branch it was written on to the current branch",
    AskAgent Agents "ask_agent" "ask agent" "ask a coding agent, through the agent plugin dffy-agent, about the focused thread: write your question in a reply box, which enter sends with the thread to the agent that answered last, else the default one; enter on the empty box asks about the thread as it is, and esc keeps the question without asking. Elsewhere, write the question on the selected lines, or the line v would start on, in a text box, and enter asks. The answer comes as the agent's reply, with a suggestion when it proposes a change; esc on the thread stops it. A comment you write starting @ and an agent's name, as @name, asks that agent too (experimental, see agent)",
    Reply Comments "reply" "reply to thread" "on a thread: reply in a text box under it; enter sends it",
    EditDraft Comments "edit_draft" "edit thread draft" "on a thread: edit your last draft in it, in a text box; enter sends it",
    DeleteDraft Comments "delete_draft" "delete thread draft" "on a thread: delete your last draft in it, the whole thread when that's its first comment or the thread is resolved with nothing on the host, once the key comes again",
    Resolve Comments "resolve" "resolve thread" "on a thread: resolve it, or reopen it",
    Adopt Comments "adopt" "make it yours" "on a thread: someone else's comments in it not posted, as an agent's, become your drafts, which posting sends under your name",
    ApplySuggestion Comments "apply_suggestion" "apply suggestion" "on a thread: put its suggestion, the one last clicked or else its last, in place of the lines it's on in the working tree, refused when they changed since; an unstaged change, which discard undoes. On one applied already, put back the lines it replaced",
    FixWithAgent Agents "fix_with_agent" "fix with agent" "on a thread, or the selected lines with a note you write first: start your coding agent in a new tab, or here until it exits, pointed at the thread to change what it asks and reply in it",
    SendOpenThreads Agents "send_open_threads" "send open threads" "take a checkpoint, then start your coding agent as fix with agent does, pointed at every open thread of yours in view (those you started or replied in) to change what each asks and reply in each; since checkpoint then shows its turn",
    OpenOnHost Comments "open_on_host" "open thread on host" "on a thread: open it on its code host, in the browser",
    ClearResolved Comments "clear_resolved" "delete resolved threads" "delete the resolved threads shown, after a y/n; on a code host's review only what isn't posted, as the host's threads would come back",
    CommentInEditor Comments "comment_in_editor" "continue in editor" "write in $EDITOR: your draft in the focused thread, else a reply to it, else a comment on the line; ctrl+g does it from the text box",
    CommentedFiles Files "commented_files" "files with comments" "the tree shows only the files with comments, or every file; f/F and n/N keep to them. In the comments panel, it steps through all threads, open ones, drafts and resolved ones, and its capital steps back",
    ReviewPull Review "review_pull" "update the review" "on a review a review plugin opened, fetch its new commits and threads and show them in place (experimental, see review)",
    ReviewPost Review "review_post" "post the review" "on a review a review plugin opened, open the dialog that posts your drafts, replies and resolves as a review (experimental, see review)",
    OpenReview Review "open_review" "open a review…" "list the reviews the review plugin offers, such as this branch's, yours and those waiting on you, to open one (experimental, see review)",
    SinceLastReview Review "since_last_review" "since last review" "on a review a review plugin opened, show only what changed since the head of your last review, as the review plugin records it, or the whole review again",
    WholeReview Review "whole_review" "show the whole review" "on the branch's review that plain dffy found, show the whole review against the working tree, or the working tree's changes again",
    Help General "help" "show this help" "show or hide this help",
    Quit General "quit" "quit" "quit",
}

/// Where a binding applies: the diff (and the tree, for keys its own
/// table leaves alone), the tree first, or the command panel the leader key
/// opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Table {
    Main,
    Tree,
    Thread,
    Conflict,
    /// While a go-to-definition peek has the keys.
    Peek,
    Leader,
}

impl Table {
    const ALL: [Self; 6] = [
        Self::Main,
        Self::Tree,
        Self::Thread,
        Self::Conflict,
        Self::Peek,
        Self::Leader,
    ];

    /// The prefix of its keys in `keybind` lines.
    fn prefix(self) -> &'static str {
        match self {
            Self::Main => "",
            Self::Tree => "tree:",
            Self::Thread => "thread:",
            Self::Conflict => "conflict:",
            Self::Peek => "peek:",
            Self::Leader => "leader:",
        }
    }
}

use Table::{Conflict as Cf, Leader as L, Main as M, Peek as P, Thread as Th, Tree as T};

/// The defaults: `(table, key, action)`. The leader's are in the order the
/// command panel lists them within a group.
const DEFAULTS: &[(Table, &str, Action)] = &[
    (M, "space", Action::Leader),
    (M, "j", Action::ScrollDown),
    (M, "down", Action::ScrollDown),
    (M, "enter", Action::ScrollDown),
    (M, "ctrl+e", Action::ScrollDown),
    (M, "ctrl+n", Action::ScrollDown),
    (M, "k", Action::ScrollUp),
    (M, "up", Action::ScrollUp),
    (M, "ctrl+y", Action::ScrollUp),
    (M, "ctrl+p", Action::ScrollUp),
    (M, "page_down", Action::PageDown),
    (M, "ctrl+f", Action::PageDown),
    (M, "b", Action::PageUp),
    (M, "page_up", Action::PageUp),
    (M, "ctrl+b", Action::PageUp),
    (M, "ctrl+d", Action::HalfPageDown),
    (M, "ctrl+u", Action::HalfPageUp),
    (M, "g", Action::Top),
    (M, "home", Action::Top),
    (M, "G", Action::Bottom),
    (M, "end", Action::Bottom),
    (M, "n", Action::NextChange),
    (M, "}", Action::NextChange),
    (M, "N", Action::PrevChange),
    (M, "{", Action::PrevChange),
    (M, "f", Action::NextFile),
    (M, "]", Action::NextFile),
    // Option+arrows: macOS terminals send them as alt+f/alt+b (word moves),
    // others as alt+right/alt+left. Unbound, alt+b would fall back to `b`.
    (M, "alt+f", Action::NextFile),
    (M, "alt+right", Action::NextFile),
    (M, "F", Action::PrevFile),
    (M, "[", Action::PrevFile),
    (M, "alt+b", Action::PrevFile),
    (M, "alt+left", Action::PrevFile),
    (M, "x", Action::Expand),
    (M, "X", Action::ToggleFull),
    (M, "l", Action::ToggleFileView),
    (M, ">", Action::ScrollRight),
    (M, "L", Action::ScrollRight),
    (M, "<", Action::ScrollLeft),
    (M, "H", Action::ScrollLeft),
    (M, "0", Action::ScrollReset),
    (M, "v", Action::Select),
    (M, "y", Action::Copy),
    (M, "Y", Action::CopyPath),
    (M, "esc", Action::Cancel),
    (M, "tab", Action::SwitchPane),
    (M, "shift+tab", Action::SwitchPane),
    (M, "T", Action::ToggleTree),
    (M, "t", Action::ToggleTree),
    (M, "o", Action::OpenPicker),
    (M, "/", Action::Search),
    (T, "/", Action::FilterTree),
    (M, "u", Action::ParentFolder),
    (M, "K", Action::GoToDefinition),
    (M, "backspace", Action::JumpBack),
    (M, "\\", Action::JumpForward),
    (P, "j", Action::ScrollDown),
    (P, "down", Action::ScrollDown),
    (P, "k", Action::ScrollUp),
    (P, "up", Action::ScrollUp),
    (P, "enter", Action::Open),
    (P, "d", Action::ShowInDiff),
    (P, "s", Action::OtherVersion),
    (P, "r", Action::References),
    (P, "m", Action::MoreUses),
    (P, "c", Action::CallsOnly),
    (P, "i", Action::UsesInDiff),
    (P, "esc", Action::ClosePeek),
    (P, "q", Action::ClosePeek),
    (T, "enter", Action::Open),
    (T, "l", Action::Open),
    (T, "right", Action::Open),
    (T, "h", Action::Collapse),
    (T, "left", Action::Collapse),
    (T, "v", Action::MarkViewed),
    (T, "x", Action::CloseFile),
    (T, "a", Action::ToggleAllFiles),
    (T, "+", Action::TreeWider),
    (T, "-", Action::TreeNarrower),
    (M, "s", Action::CycleLayout),
    (M, "W", Action::ToggleWrap),
    (M, "d", Action::ToggleSyntax),
    (M, "r", Action::ToggleRendered),
    (M, "w", Action::ToggleWatch),
    (M, "'", Action::SinceCheckpoint),
    (M, "A", Action::Stage),
    (M, "U", Action::Unstage),
    (M, "D", Action::Discard),
    (Cf, "n", Action::NextConflict),
    (Cf, "N", Action::PrevConflict),
    (Cf, "o", Action::KeepCurrent),
    (Cf, "t", Action::TakeIncoming),
    (Cf, "b", Action::KeepBoth),
    (Cf, "a", Action::TakeBase),
    (Cf, "w", Action::WriteResolution),
    (M, "e", Action::Edit),
    (M, "c", Action::Comment),
    (M, "S", Action::SuggestChange),
    (M, "C", Action::CommentFile),
    (M, "i", Action::Comments),
    (M, "m", Action::NextThread),
    (M, "M", Action::PrevThread),
    (M, "z", Action::ToggleThread),
    (M, "Z", Action::ToggleThreads),
    (M, "V", Action::MoveThread),
    (M, "a", Action::AskAgent),
    (M, "E", Action::CommentInEditor),
    // In the order of the actions, as `dffy config --default` lists them.
    (Th, "y", Action::CopyPrompt),
    (Th, "a", Action::AskAgent),
    (Th, "r", Action::Reply),
    (Th, "e", Action::EditDraft),
    (Th, "d", Action::DeleteDraft),
    (Th, "x", Action::Resolve),
    (Th, "A", Action::Adopt),
    (Th, "s", Action::ApplySuggestion),
    (Th, "f", Action::FixWithAgent),
    (Th, "o", Action::OpenOnHost),
    (T, "c", Action::CommentedFiles),
    (M, "p", Action::ReviewPull),
    (M, "P", Action::ReviewPost),
    (M, "O", Action::OpenReview),
    (M, "R", Action::SinceLastReview),
    (M, "?", Action::Help),
    (M, "q", Action::Quit),
    (M, "ctrl+c", Action::Quit),
    // A key here that the diff binds too means what it means there
    // (`a_leader_key_that_main_binds_runs_mains_action`); the rest take
    // keys the diff leaves free, a symbol when no letter is. Not `h`/`l`:
    // the panel steps values with those; not `/`: it filters its rows with
    // that, so searching is the diff's `/`.
    (L, "s", Action::CycleLayout),
    (L, "W", Action::ToggleWrap),
    (L, "X", Action::ToggleFull),
    (L, "_", Action::ToggleFileView),
    (L, "x", Action::Expand),
    (L, "B", Action::ToggleStatusBar),
    (L, "=", Action::ToggleAllFiles),
    (L, "d", Action::ToggleSyntax),
    (L, "r", Action::ToggleRendered),
    (L, "I", Action::ToggleWhitespace),
    (L, "*", Action::ToggleHighlight),
    (L, ",", Action::CycleIndicators),
    (L, "%", Action::CycleModifiedLines),
    (L, "#", Action::ToggleLineNumbers),
    (L, "@", Action::ToggleHunkHeaders),
    (L, "o", Action::OpenPicker),
    (L, "K", Action::GoToDefinition),
    (L, "(", Action::JumpBack),
    (L, "\\", Action::JumpForward),
    (L, "|", Action::FilterTree),
    (L, "!", Action::CommentedFiles),
    (L, ")", Action::CloseFile),
    (L, "t", Action::ToggleTree),
    (L, "f", Action::NextFile),
    (L, "F", Action::PrevFile),
    (L, "y", Action::Copy),
    (L, "Y", Action::CopyPath),
    (L, "\"", Action::CopyPrompt),
    (L, "J", Action::ToggleCopyDecorations),
    (L, "A", Action::Stage),
    (L, "U", Action::Unstage),
    (L, "D", Action::Discard),
    (L, "$", Action::ToggleMouse),
    (L, "w", Action::ToggleWatch),
    (L, ";", Action::TakeCheckpoint),
    (L, "'", Action::SinceCheckpoint),
    (L, "~", Action::CycleTheme),
    (L, "i", Action::Comments),
    (L, "E", Action::CommentInEditor),
    (L, "a", Action::AskAgent),
    (L, "&", Action::FixWithAgent),
    (L, "^", Action::SendOpenThreads),
    (L, "-", Action::ClearResolved),
    (L, ".", Action::OpenTerminal),
    (L, "S", Action::SuggestChange),
    (L, "O", Action::OpenReview),
    (L, "R", Action::SinceLastReview),
    (L, "Q", Action::WholeReview),
    (L, "?", Action::Help),
];

/// The kinds of action, in order: the sections of docs/keys.md, and the
/// groups the `?` overlay and the command panel list under their tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Group {
    Navigation,
    Peek,
    Files,
    Layout,
    Diff,
    Display,
    Copy,
    Changes,
    Modes,
    Comments,
    Agents,
    Review,
    General,
}

impl Group {
    pub(crate) const ALL: [Self; 13] = [
        Self::Navigation,
        Self::Peek,
        Self::Files,
        Self::Layout,
        Self::Diff,
        Self::Display,
        Self::Copy,
        Self::Changes,
        Self::Modes,
        Self::Comments,
        Self::Agents,
        Self::Review,
        Self::General,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Navigation => "Navigation",
            Self::Peek => "In a peek",
            Self::Files => "Files/tree",
            Self::Layout => "Layout",
            Self::Diff => "Diff",
            Self::Display => "Display",
            Self::Copy => "Selection & copy",
            Self::Changes => "Stage, discard & resolve",
            Self::Modes => "Watch & checkpoints",
            Self::Comments => "Comments",
            Self::Agents => "Agents",
            Self::Review => "Review",
            Self::General => "General",
        }
    }

    /// The tab it's listed in, in `?` and the command panel; a tab of
    /// several groups titles each with its name.
    pub(crate) fn tab(self) -> Tab {
        match self {
            Self::Navigation | Self::Peek => Tab::Move,
            Self::Files => Tab::Files,
            Self::Layout | Self::Diff | Self::Display => Tab::View,
            Self::Changes | Self::Modes => Tab::Git,
            Self::Copy => Tab::Copy,
            Self::Comments | Self::Agents | Self::Review => Tab::Review,
            Self::General => Tab::General,
        }
    }
}

/// The tabs of `?` and the command panel, each listing those it has room
/// for in an order of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tab {
    Move,
    Files,
    View,
    Git,
    Copy,
    Review,
    General,
}

impl Tab {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Move => "Move",
            Self::Files => "Files",
            Self::View => "View",
            Self::Git => "Git",
            Self::Copy => "Copy",
            Self::Review => "Review",
            Self::General => "General",
        }
    }
}

impl Action {
    /// The command panel's name for it; settings with a value get it
    /// after a colon there.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::OpenPicker => "Find file…",
            Self::GoToDefinition => "Go to definition…",
            Self::JumpBack => "Back",
            Self::JumpForward => "Forward",
            Self::FilterTree => "Filter tree…",
            Self::Search => "Search in diff…",
            Self::ToggleTree => "File tree",
            Self::NextFile => "Next file",
            Self::PrevFile => "Previous file",
            Self::CycleLayout => "Layout",
            Self::ToggleWrap => "Wrap long lines",
            Self::ToggleSyntax => "Syntax diff",
            Self::ToggleRendered => "Rendered view",
            Self::Expand => "More context",
            Self::ToggleFull => "Whole file",
            Self::ToggleFileView => "File view",
            Self::CycleIndicators => "Indicators",
            Self::CycleModifiedLines => "Modified lines",
            Self::ToggleHighlight => "Syntax highlighting",
            Self::ToggleWhitespace => "Ignore whitespace",
            Self::ToggleLineNumbers => "Line numbers",
            Self::ToggleHunkHeaders => "Hunk headers",
            Self::ToggleStatusBar => "Status bar",
            Self::CycleTheme => "Theme",
            Self::Copy => "Copy selection",
            Self::CopyPath => "Copy file path",
            Self::CopyAbsolutePath => "Copy absolute path",
            Self::CopyComment => "Copy comment",
            Self::CopyThread => "Copy thread",
            Self::CopyPrompt => "Copy as prompt",
            Self::ToggleCopyDecorations => "Copy with line numbers and signs",
            Self::Stage => "Stage lines",
            Self::Unstage => "Unstage lines",
            Self::Discard => "Discard lines…",
            Self::NextConflict => "Next conflict",
            Self::PrevConflict => "Previous conflict",
            Self::KeepCurrent => "Keep current (ours)",
            Self::TakeIncoming => "Take incoming (theirs)",
            Self::KeepBoth => "Keep both",
            Self::TakeBase => "Take base",
            Self::WriteResolution => "Write resolved file…",
            Self::ToggleMouse => "Mouse selection",
            Self::ToggleWatch => "Watch for changes",
            Self::TakeCheckpoint => "Take checkpoint",
            Self::SinceCheckpoint => "Since checkpoint",
            Self::CommentInEditor => "Continue in editor",
            Self::Adopt => "Make it yours",
            Self::OpenTerminal => "Open terminal here",
            Self::SuggestChange => "Suggest change",
            Self::CommentedFiles => "Files with comments",
            Self::CloseFile => "Close file",
            Self::ToggleAllFiles => "All files",
            Self::Comments => "Comments panel",
            Self::OpenReview => "Open review…",
            Self::SinceLastReview => "Since last review",
            Self::WholeReview => "Show whole review",
            Self::ReviewPost => "Post review…",
            Self::ReviewPull => "Pull updates",
            Self::MoveThread => "Move thread to this branch",
            Self::AskAgent => "Ask agent",
            Self::FixWithAgent => "Fix with agent",
            Self::SendOpenThreads => "Send open threads to agent",
            Self::ClearResolved => "Delete resolved threads…",
            Self::Help => "All keys",
            other => other.label(),
        }
    }

    /// A few words for notes on the status line.
    #[expect(clippy::too_many_lines, reason = "a few words per action")]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Leader => "command panel",
            Self::ScrollDown => "down",
            Self::ScrollUp => "up",
            Self::PageDown => "page down",
            Self::PageUp => "page up",
            Self::HalfPageDown => "half page down",
            Self::HalfPageUp => "half page up",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::NextChange => "next change",
            Self::PrevChange => "previous change",
            Self::NextFile => "next file",
            Self::PrevFile => "previous file",
            Self::Expand => "more context",
            Self::ToggleFull => "whole file",
            Self::ToggleFileView => "file view",
            Self::ScrollRight => "scroll right",
            Self::ScrollLeft => "scroll left",
            Self::ScrollReset => "scroll back",
            Self::Select => "select lines",
            Self::Copy => "copy selection",
            Self::CopyPath => "copy path",
            Self::CopyAbsolutePath => "copy absolute path",
            Self::CopyComment => "copy comment",
            Self::CopyThread => "copy thread",
            Self::CopyPrompt => "copy as prompt",
            Self::Cancel => "cancel",
            Self::SwitchPane => "switch pane",
            Self::ToggleTree => "tree",
            Self::OpenPicker => "find file",
            Self::Search => "search",
            Self::FilterTree => "filter",
            Self::Open => "open",
            Self::Collapse => "collapse",
            Self::ParentFolder => "parent folder",
            Self::GoToDefinition => "go to definition",
            Self::ShowInDiff => "show in diff",
            Self::OtherVersion => "other version",
            Self::References => "definitions/uses",
            Self::MoreUses => "more uses",
            Self::CallsOnly => "calls only",
            Self::UsesInDiff => "uses in diff",
            Self::ClosePeek => "close peek",
            Self::JumpBack => "jump back",
            Self::JumpForward => "jump forward",
            Self::MarkViewed => "mark viewed",
            Self::CloseFile => "close file",
            Self::ToggleAllFiles => "all files",
            Self::TreeWider => "wider tree",
            Self::TreeNarrower => "narrower tree",
            Self::CycleLayout => "layout",
            Self::ToggleWrap => "wrap",
            Self::ToggleSyntax => "syntax diff",
            Self::ToggleRendered => "rendered view",
            Self::CycleIndicators => "indicators",
            Self::CycleModifiedLines => "modified lines",
            Self::ToggleHighlight => "highlighting",
            Self::ToggleWhitespace => "ignore whitespace",
            Self::ToggleLineNumbers => "line numbers",
            Self::ToggleHunkHeaders => "hunk headers",
            Self::ToggleStatusBar => "status bar at the top",
            Self::CycleTheme => "theme",
            Self::ToggleWatch => "watch",
            Self::TakeCheckpoint => "take checkpoint",
            Self::SinceCheckpoint => "since checkpoint",
            Self::ToggleMouse => "mouse",
            Self::ToggleCopyDecorations => "copy decorations",
            Self::Stage => "stage",
            Self::Unstage => "unstage",
            Self::Discard => "discard",
            Self::NextConflict => "next conflict",
            Self::PrevConflict => "previous conflict",
            Self::KeepCurrent => "keep current",
            Self::TakeIncoming => "take incoming",
            Self::KeepBoth => "keep both",
            Self::TakeBase => "take base",
            Self::WriteResolution => "write resolved file",
            Self::Edit => "edit",
            Self::OpenTerminal => "open terminal",
            Self::Comment => "comment",
            Self::SuggestChange => "suggest change",
            Self::CommentFile => "comment on file",
            Self::Comments => "comments",
            Self::NextThread => "next thread",
            Self::PrevThread => "previous thread",
            Self::ToggleThread => "fold thread",
            Self::ToggleThreads => "fold threads",
            Self::MoveThread => "move thread here",
            Self::AskAgent => "ask agent",
            Self::Reply => "reply",
            Self::EditDraft => "edit draft",
            Self::DeleteDraft => "delete draft",
            Self::Resolve => "resolve",
            Self::Adopt => "make it yours",
            Self::ApplySuggestion => "apply suggestion",
            Self::FixWithAgent => "fix with agent",
            Self::SendOpenThreads => "send open threads",
            Self::OpenOnHost => "open on host",
            Self::ClearResolved => "delete resolved threads",
            Self::CommentInEditor => "continue in editor",
            Self::CommentedFiles => "files with comments",
            Self::ReviewPull => "update review",
            Self::ReviewPost => "post review",
            Self::OpenReview => "open review",
            Self::SinceLastReview => "since last review",
            Self::WholeReview => "whole review",
            Self::Help => "all keys",
            Self::Quit => "quit",
        }
    }
}

/// A key with its modifiers, as bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chord {
    pub key: Key,
    pub mods: Mods,
}

const NAMED: &[(&str, Key)] = &[
    ("space", Key::Char(' ')),
    ("enter", Key::Enter),
    ("tab", Key::Tab),
    ("backspace", Key::Backspace),
    ("esc", Key::Esc),
    ("up", Key::Up),
    ("down", Key::Down),
    ("left", Key::Left),
    ("right", Key::Right),
    ("home", Key::Home),
    ("end", Key::End),
    ("page_up", Key::PageUp),
    ("page_down", Key::PageDown),
    ("insert", Key::Insert),
    ("delete", Key::Delete),
    // Accepted too; the names above are the ones shown.
    ("escape", Key::Esc),
    ("return", Key::Enter),
    ("pgup", Key::PageUp),
    ("pgdn", Key::PageDown),
];

impl Chord {
    /// Parses `ctrl+d`, `alt+x`, `shift+tab`, `N`, `<`, `space`, `f5`...
    fn parse(text: &str) -> Result<Self, Invalid> {
        let mut mods = Mods::NONE;
        let mut shift = false;
        let mut rest = text;
        // A lone `+` is the key itself, never a separator.
        while rest.chars().nth(1).is_some() {
            if let Some(r) = rest.strip_prefix("ctrl+") {
                mods = mods.with(Mods::CTRL);
                rest = r;
            } else if let Some(r) = rest.strip_prefix("alt+") {
                mods = mods.with(Mods::ALT);
                rest = r;
            } else if let Some(r) = rest.strip_prefix("shift+") {
                shift = true;
                rest = r;
            } else {
                break;
            }
        }
        if rest.is_empty() {
            return Err(Invalid {
                message: format!("'{text}' names no key"),
                hint: Some("put the key after the modifier, as in ctrl+d".to_owned()),
            });
        }
        let mut chars = rest.chars();
        let key = match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Key::Char(c),
            _ => match NAMED.iter().find(|(name, _)| *name == rest) {
                Some(&(_, key)) => key,
                None => match rest.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                    Some(n @ 1..=12) => Key::F(n),
                    _ => return Err(unknown_key(rest)),
                },
            },
        };
        let key = match (key, shift) {
            (key, false) => key,
            (Key::Tab, true) => Key::BackTab,
            (Key::Char(c), true) if c.is_ascii_alphabetic() => Key::Char(c.to_ascii_uppercase()),
            (_, true) => {
                return Err(Invalid {
                    message: format!("'{text}' can't be told apart from its key without shift"),
                    hint: Some("bind the shifted character itself, like N or ?".to_owned()),
                });
            }
        };
        // Terminals send ctrl with a letter as one byte, whatever the case.
        let key = match key {
            Key::Char(c) if mods.contains(Mods::CTRL) => Key::Char(c.to_ascii_lowercase()),
            key => key,
        };
        Ok(Self { key, mods })
    }

    /// The name [`Chord::parse`] reads back.
    pub(crate) fn name(self) -> String {
        let mut out = String::new();
        if self.mods.contains(Mods::CTRL) {
            out.push_str("ctrl+");
        }
        if self.mods.contains(Mods::ALT) {
            out.push_str("alt+");
        }
        match self.key {
            Key::BackTab => out.push_str("shift+tab"),
            Key::F(n) => {
                out.push('f');
                out.push_str(&n.to_string());
            }
            key => match NAMED.iter().find(|(_, k)| *k == key) {
                Some((name, _)) => out.push_str(name),
                None => {
                    if let Key::Char(c) = key {
                        out.push(c);
                    }
                }
            },
        }
        out
    }
}

fn unknown_key(name: &str) -> Invalid {
    let names = NAMED.iter().map(|(n, _)| *n);
    Invalid {
        message: format!("unknown key '{name}'"),
        hint: Some(match dffy_core::suggest::closest(name, names) {
            Some(close) => format!("did you mean '{close}'?"),
            None => "keys are a character as typed (n, N, <, ?), a name (space, enter, tab, \
                     esc, up, page_down, f1...), with ctrl+ or alt+ before it"
                .to_owned(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Keys {
    main: Vec<(Chord, Action)>,
    tree: Vec<(Chord, Action)>,
    thread: Vec<(Chord, Action)>,
    conflict: Vec<(Chord, Action)>,
    peek: Vec<(Chord, Action)>,
    leader: Vec<(Chord, Action)>,
}

impl Default for Keys {
    #[expect(
        clippy::expect_used,
        reason = "DEFAULTS is a constant table, and every_default_key_reads_back_from_its_name parses all of it"
    )]
    fn default() -> Self {
        let mut keys = Self {
            main: Vec::new(),
            tree: Vec::new(),
            thread: Vec::new(),
            conflict: Vec::new(),
            peek: Vec::new(),
            leader: Vec::new(),
        };
        for &(table, key, action) in DEFAULTS {
            let chord = Chord::parse(key).expect("default keys parse");
            keys.table_mut(table).push((chord, action));
        }
        keys
    }
}

impl Keys {
    fn table_mut(&mut self, table: Table) -> &mut Vec<(Chord, Action)> {
        match table {
            Table::Main => &mut self.main,
            Table::Tree => &mut self.tree,
            Table::Thread => &mut self.thread,
            Table::Conflict => &mut self.conflict,
            Table::Peek => &mut self.peek,
            Table::Leader => &mut self.leader,
        }
    }

    fn table(&self, table: Table) -> &[(Chord, Action)] {
        match table {
            Table::Main => &self.main,
            Table::Tree => &self.tree,
            Table::Thread => &self.thread,
            Table::Conflict => &self.conflict,
            Table::Peek => &self.peek,
            Table::Leader => &self.leader,
        }
    }

    /// The command panel's entries, in order.
    pub(crate) fn leader_entries(&self) -> &[(Chord, Action)] {
        &self.leader
    }

    /// Applies a `keybind` value: `<key>=<action>`, `<key>=unbind`, either
    /// with `tree:` before the key for the tree's table, `thread:` for the
    /// thread's, `conflict:` for the conflict's, `peek:` for a peek's or
    /// `leader:` for the command panel.
    pub(crate) fn bind(&mut self, value: &str) -> Result<(), Invalid> {
        // At the last `=`, so `==` binds the `=` key.
        let Some((key, action)) = value.rsplit_once('=').filter(|(k, _)| !k.is_empty()) else {
            return Err(Invalid {
                message: format!("expected <key>=<action>, found '{value}'"),
                hint: Some("for example: keybind = ctrl+d=half_page_down".to_owned()),
            });
        };
        let tables = [
            Table::Tree,
            Table::Thread,
            Table::Conflict,
            Table::Peek,
            Table::Leader,
        ];
        let (table, key) = tables
            .into_iter()
            .find_map(|t| Some((t, key.strip_prefix(t.prefix()).filter(|k| !k.is_empty())?)))
            .unwrap_or((Table::Main, key));
        let chord = Chord::parse(key.trim())?;
        let action = action.trim();
        let action = match Action::ALL.iter().find(|a| a.name() == action) {
            Some(&a) => Some(a),
            None if action == "unbind" => None,
            None => {
                let names = Action::ALL.iter().map(|a| a.name()).chain(["unbind"]);
                return Err(Invalid {
                    message: format!("unknown action '{action}'"),
                    hint: Some(match dffy_core::suggest::closest(action, names) {
                        Some(close) => format!("did you mean '{close}'?"),
                        None => "dffy config --default lists every action".to_owned(),
                    }),
                });
            }
        };
        let leader = table == Table::Leader;
        let table = self.table_mut(table);
        match (table.iter().position(|(c, _)| *c == chord), action) {
            // In place, so the command panel keeps its order.
            (Some(i), Some(action)) if leader => table[i].1 = action,
            (Some(i), action) => {
                table.remove(i);
                table.extend(action.map(|a| (chord, a)));
            }
            (None, action) => table.extend(action.map(|a| (chord, a))),
        }
        Ok(())
    }

    /// The action of `key` with `mods`, looked up in the tree's table first
    /// when `tree`. Keys arriving with alt or shift and no binding of their
    /// own act as the plain key, as they did before keys were bindable.
    pub(crate) fn action(&self, tree: bool, key: Key, mods: Mods) -> Option<Action> {
        let find = |mods: Mods| {
            let chord = Chord { key, mods };
            let lookup = |table: &[(Chord, Action)]| {
                table.iter().find(|(c, _)| *c == chord).map(|&(_, a)| a)
            };
            tree.then(|| lookup(&self.tree))
                .flatten()
                .or_else(|| lookup(&self.main))
        };
        find(mods).or_else(|| {
            (!mods.contains(Mods::CTRL) && mods != Mods::NONE)
                .then(|| find(Mods::NONE))
                .flatten()
        })
    }

    /// The action of `key` with `mods` while a thread has the keys, which
    /// takes the key before [`Keys::action`]: only a key bound exactly.
    pub(crate) fn thread_action(&self, key: Key, mods: Mods) -> Option<Action> {
        let chord = Chord { key, mods };
        (self.thread.iter())
            .find(|(c, _)| *c == chord)
            .map(|&(_, a)| a)
    }

    /// The keys bound to `action`, in the main table or the tree's.
    pub(crate) fn chords(&self, tree: bool, action: Action) -> impl Iterator<Item = Chord> + '_ {
        self.in_table(if tree { Table::Tree } else { Table::Main }, action)
    }

    /// The keys bound to `action` while a thread has the keys.
    pub(crate) fn thread_chords(&self, action: Action) -> impl Iterator<Item = Chord> + '_ {
        self.in_table(Table::Thread, action)
    }

    /// The action of `key` with `mods` while the file at the cursor has
    /// conflicts to resolve, which takes the key after a thread's and
    /// before [`Keys::action`]: only a key bound exactly.
    pub(crate) fn conflict_action(&self, key: Key, mods: Mods) -> Option<Action> {
        let chord = Chord { key, mods };
        (self.conflict.iter())
            .find(|(c, _)| *c == chord)
            .map(|&(_, a)| a)
    }

    /// The keys bound to `action` in a file with conflicts to resolve.
    pub(crate) fn conflict_chords(&self, action: Action) -> impl Iterator<Item = Chord> + '_ {
        self.in_table(Table::Conflict, action)
    }

    /// The action of `key` with `mods` while a peek has the keys, which
    /// takes the key before every other table: only a key bound exactly.
    pub(crate) fn peek_action(&self, key: Key, mods: Mods) -> Option<Action> {
        let chord = Chord { key, mods };
        (self.peek.iter())
            .find(|(c, _)| *c == chord)
            .map(|&(_, a)| a)
    }

    /// The keys bound to `action` while a peek has the keys.
    pub(crate) fn peek_chords(&self, action: Action) -> impl Iterator<Item = Chord> + '_ {
        self.in_table(Table::Peek, action)
    }

    fn in_table(&self, table: Table, action: Action) -> impl Iterator<Item = Chord> + '_ {
        self.table(table)
            .iter()
            .filter(move |(_, a)| *a == action)
            .map(|&(c, _)| c)
    }

    /// `keybind` lines that turn the defaults into these bindings.
    pub(crate) fn render_changes(&self, c: &mut Canvas) {
        let defaults = Self::default();
        for table in Table::ALL {
            let (ours, theirs) = (self.table(table), defaults.table(table));
            for (chord, _) in theirs {
                if !ours.iter().any(|(c, _)| c == chord) {
                    line(c, table, *chord, "unbind");
                }
            }
            for binding in ours {
                if !theirs.contains(binding) {
                    line(c, table, binding.0, binding.1.name());
                }
            }
        }
    }
}

fn line(c: &mut Canvas, table: Table, chord: Chord, action: &str) {
    c.push(Role::Hint, "keybind");
    c.push(Role::Dim, " = ");
    c.push(
        Role::Plain,
        &format!("{}{}={action}", table.prefix(), chord.name()),
    );
    c.end_line();
}

/// Every action with its default keys, as commented config lines.
pub(crate) fn render_defaults(c: &mut Canvas) {
    c.line(
        Role::Dim,
        "# Keys in the viewer: keybind = <key>=<action>, or <key>=unbind to free a key. \
         A key is a character as typed (n, N, <, ?) or a name (space, enter, tab, shift+tab, \
         esc, backspace, up, down, left, right, home, end, page_up, page_down, insert, delete, \
         f1 to f12), with ctrl+ or alt+ before it. Keys pressed in the file tree look in its \
         own table first, set with tree: before the key (keybind = tree:v=mark_viewed), and so \
         do keys pressed while a comment thread is focused, in the thread's table (thread:), \
         then while the file at the cursor has conflicts to resolve, in the conflict's table \
         (conflict:). Keys pressed while a go-to-definition peek has the keys look in its table \
         (peek:) before any other. The command panel the leader key opens has its own table too, set with \
         leader: (keybind = leader:z=toggle_wrap); within a group, the panel lists its entries in this order.",
    );
    let defaults = Keys::default();
    for &action in Action::ALL {
        c.end_line();
        c.line(Role::Dim, &format!("# {}: {}", action.name(), action.doc()));
        let mut any = false;
        for table in Table::ALL {
            for &(chord, a) in defaults.table(table) {
                if a == action {
                    line(c, table, chord, action.name());
                    any = true;
                }
            }
        }
        if !any {
            c.line(Role::Dim, "# (no key by default)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_key_reads_back_from_its_name() {
        let keys = Keys::default();
        for (chord, _) in (keys.main.iter())
            .chain(&keys.tree)
            .chain(&keys.thread)
            .chain(&keys.conflict)
            .chain(&keys.peek)
            .chain(&keys.leader)
        {
            assert_eq!(Chord::parse(&chord.name()).ok(), Some(*chord), "{chord:?}");
        }
        assert_eq!(keys.chords(false, Action::Quit).count(), 2);
    }

    /// A key of the command panel that the diff binds too runs the same
    /// action in both, so a key learnt in one means the same in the other.
    #[test]
    fn a_leader_key_that_main_binds_runs_mains_action() {
        let keys = Keys::default();
        let differ: Vec<String> = (keys.leader.iter())
            .filter_map(|&(chord, action)| {
                let (_, main) = keys.main.iter().find(|(c, _)| *c == chord)?;
                (*main != action).then(|| {
                    format!(
                        "{}: {} here, {} in the diff",
                        chord.name(),
                        action.name(),
                        main.name()
                    )
                })
            })
            .collect();
        assert!(differ.is_empty(), "{differ:#?}");
    }

    /// A conflict's key needs its exact chord, as a thread's does.
    #[test]
    fn conflict_keybinds_take_their_prefix() {
        let mut keys = Keys::default();
        for value in ["conflict:o=unbind", "conflict:O=keep_current"] {
            keys.bind(value)
                .unwrap_or_else(|e| panic!("{value}: {e:?}"));
        }
        let conflict = |c| keys.conflict_action(Key::Char(c), Mods::NONE);
        assert_eq!(
            (conflict('O'), conflict('o'), conflict('t')),
            (Some(Action::KeepCurrent), None, Some(Action::TakeIncoming))
        );
    }

    /// Within a table, no key means two things.
    #[test]
    fn no_key_is_bound_twice_in_a_table() {
        let keys = Keys::default();
        for table in Table::ALL {
            let chords = keys.table(table);
            for (i, (chord, _)) in chords.iter().enumerate() {
                assert!(
                    !chords[..i].iter().any(|(c, _)| c == chord),
                    "{}{} twice",
                    table.prefix(),
                    chord.name()
                );
            }
        }
    }

    /// The `?` overlay fits two columns of key and label in about 90
    /// columns only while every label stays this short.
    #[test]
    fn every_action_has_a_short_label_for_the_overlay() {
        for &action in Action::ALL {
            let short = action.short();
            let width = unicode_width::UnicodeWidthStr::width(short);
            assert!(
                (1..=24).contains(&width),
                "{}: '{short}' is {width} columns",
                action.name()
            );
        }
    }

    #[test]
    fn keybind_lines_remap_unbind_and_take_tree_bindings() {
        let mut keys = Keys::default();
        for value in [
            "n=next_file",
            "ctrl+d=page_down",
            "q=unbind",
            "tree:v=unbind",
            "tree:x=mark_viewed",
            "==help",
            "ctrl++=tree_wider",
            "shift+tab=unbind",
            "alt+j=bottom",
            "pgdn=bottom",
            "leader:z=toggle_wrap",
            "leader:@=unbind",
            "thread:r=unbind",
            "thread:R=reply",
            "conflict:o=unbind",
            "conflict:O=keep_current",
        ] {
            keys.bind(value)
                .unwrap_or_else(|e| panic!("{value}: {e:?}"));
        }
        let none = Mods::NONE;
        let action = |tree, key, mods| keys.action(tree, key, mods);
        assert_eq!(action(false, Key::Char('n'), none), Some(Action::NextFile));
        assert_eq!(
            action(false, Key::Char('d'), Mods::CTRL),
            Some(Action::PageDown)
        );
        assert_eq!(action(false, Key::Char('q'), none), None);
        // Unbound in the tree's table, v falls through to the main one.
        assert_eq!(action(true, Key::Char('v'), none), Some(Action::Select));
        let leader = |c: &str| {
            keys.leader_entries()
                .iter()
                .find(|(chord, _)| chord.name() == c)
                .map(|&(_, a)| a)
        };
        assert_eq!(leader("z"), Some(Action::ToggleWrap));
        // A thread's key needs its exact chord, and falls through when unbound.
        assert_eq!(
            keys.thread_action(Key::Char('R'), none),
            Some(Action::Reply)
        );
        assert_eq!(keys.thread_action(Key::Char('r'), none), None);
        assert_eq!(keys.thread_action(Key::Char('x'), Mods::ALT), None);
        assert_eq!(
            keys.thread_action(Key::Char('x'), none),
            Some(Action::Resolve)
        );
        assert_eq!(leader("@"), None);
        assert_eq!(action(true, Key::Char('x'), none), Some(Action::MarkViewed));
        assert_eq!(action(false, Key::Char('x'), none), Some(Action::Expand));
        assert_eq!(action(false, Key::Char('='), none), Some(Action::Help));
        assert_eq!(
            action(false, Key::Char('+'), Mods::CTRL),
            Some(Action::TreeWider)
        );
        assert_eq!(action(false, Key::BackTab, none), None);
        assert_eq!(
            action(false, Key::Char('j'), Mods::ALT),
            Some(Action::Bottom)
        );
        // Unbound alt and shift combinations act as the plain key.
        assert_eq!(
            action(false, Key::Char('k'), Mods::ALT),
            Some(Action::ScrollUp)
        );
        assert_eq!(action(false, Key::PageDown, none), Some(Action::Bottom));
        // Option+arrows go between files, as alt+f/alt+b or alt+right/alt+left.
        assert_eq!(
            action(false, Key::Char('b'), Mods::ALT),
            Some(Action::PrevFile)
        );
        assert_eq!(action(true, Key::Left, Mods::ALT), Some(Action::PrevFile));
        assert_eq!(
            action(false, Key::Char('f'), Mods::ALT),
            Some(Action::NextFile)
        );
        assert_eq!(action(true, Key::Right, Mods::ALT), Some(Action::NextFile));
        // `dffy config` shows the changes as the lines that make them.
        let mut c = Canvas::default();
        keys.render_changes(&mut c);
        insta::assert_snapshot!(c.markup(), @r"
        <hint>keybind</><dim> = </>shift+tab=unbind
        <hint>keybind</><dim> = </>q=unbind
        <hint>keybind</><dim> = </>n=next_file
        <hint>keybind</><dim> = </>ctrl+d=page_down
        <hint>keybind</><dim> = </>==help
        <hint>keybind</><dim> = </>ctrl++=tree_wider
        <hint>keybind</><dim> = </>alt+j=bottom
        <hint>keybind</><dim> = </>page_down=bottom
        <hint>keybind</><dim> = </>tree:v=unbind
        <hint>keybind</><dim> = </>tree:x=mark_viewed
        <hint>keybind</><dim> = </>thread:r=unbind
        <hint>keybind</><dim> = </>thread:R=reply
        <hint>keybind</><dim> = </>conflict:o=unbind
        <hint>keybind</><dim> = </>conflict:O=keep_current
        <hint>keybind</><dim> = </>leader:@=unbind
        <hint>keybind</><dim> = </>leader:z=toggle_wrap
        ");
    }

    #[test]
    fn bad_keybinds_are_rejected_with_a_suggestion() {
        let mut keys = Keys::default();
        let errors: Vec<String> = [
            "n=nxt_change",
            "pagedown=page_down",
            "hyper+j=quit",
            "ctrl+=quit",
            "n",
            "shift+1=quit",
            "n=frobnicate",
        ]
        .iter()
        .map(|v| {
            let e = keys.bind(v).expect_err(v);
            format!("{} / {}", e.message, e.hint.unwrap_or_default())
        })
        .collect();
        insta::assert_snapshot!(errors.join("\n"), @r"
        unknown action 'nxt_change' / did you mean 'next_change'?
        unknown key 'pagedown' / did you mean 'page_down'?
        unknown key 'hyper+j' / keys are a character as typed (n, N, <, ?), a name (space, enter, tab, esc, up, page_down, f1...), with ctrl+ or alt+ before it
        'ctrl+' names no key / put the key after the modifier, as in ctrl+d
        expected <key>=<action>, found 'n' / for example: keybind = ctrl+d=half_page_down
        'shift+1' can't be told apart from its key without shift / bind the shifted character itself, like N or ?
        unknown action 'frobnicate' / dffy config --default lists every action
        ");
        assert_eq!(keys, Keys::default());
    }
}
