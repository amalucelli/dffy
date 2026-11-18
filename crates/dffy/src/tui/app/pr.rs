//! Pull request mode: the viewer on a PR that a review plugin prepared,
//! showing `<base>...<head>` with the PR's comments file. The plugin runs
//! in the background (see [`crate::plugin::review`]): what it says comes
//! back as [`Event::Review`] for the run's id, and runs that are no longer
//! wanted are ignored. Here are the state of the PR, the run going on, the
//! picker of PRs and the dialog that posts the review.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dffy_core::git::Revisions;
use dffy_core::view::Attributes;
use unicode_width::UnicodeWidthStr;

use std::fmt::Write as _;

use super::{Action, App, Comments, escape, is_web};
use crate::plugin::review::{Draft, Plan, Pr, Reason, Said, Session};
use crate::term::{Grid, Input, Key, Mods};
use crate::theme::{Role, Style, Theme};
use crate::tui::help;
use crate::tui::textbox::TextBox;

/// How often the plugin is asked whether the PR's head moved.
pub(super) const CHECK_EVERY: Duration = Duration::from_secs(60);

/// The PR shown.
pub(super) struct Mode {
    pub plugin: String,
    pub session: Session,
    /// The range is `<last_reviewed_head>..<head>`, not the whole PR.
    pub since_last: bool,
    /// Found as the branch checked out: the PR's base against the working
    /// tree, so changes not pushed yet show with the PR's comments.
    pub local: bool,
    /// The event of the last post, as an index into [`EVENTS`].
    pub event: usize,
    /// How the last post ended, which the status bar keeps saying.
    pub outcome: Option<String>,
    /// The head on the host, when the last check found it moved.
    pub moved: Option<String>,
    /// The check running, by its run's id.
    pub check: Option<u64>,
    pub next_check: Instant,
    /// Threads resolved or reopened here since the last post, by root,
    /// with the state each had before. The comments file can't tell an
    /// unsent resolve from a sent one; the plugin keeps that its own way.
    pub resolves: HashMap<String, bool>,
    /// The post dialog's summary, kept between openings until it's sent.
    pub summary: String,
}

impl Mode {
    fn new(plugin: String, session: Session) -> Self {
        Self {
            plugin,
            session,
            since_last: false,
            local: false,
            event: 0,
            outcome: None,
            moved: None,
            check: None,
            next_check: Instant::now() + CHECK_EVERY,
            resolves: HashMap::new(),
            summary: String::new(),
        }
    }

    /// The revisions shown: the PR, or what changed since the last review.
    fn revisions(&self) -> Revisions {
        let s = &self.session;
        let range = match (&s.last_reviewed_head, self.since_last) {
            (Some(last), true) => format!("{last}..{}", s.head),
            _ if self.local => {
                return Revisions {
                    revs: vec![s.base.clone()],
                    merge_base: true,
                    ..Revisions::default()
                };
            }
            _ => format!("{}...{}", s.base, s.head),
        };
        Revisions {
            revs: vec![range],
            ..Revisions::default()
        }
    }

    fn description(&self) -> String {
        let n = self.session.number;
        if self.since_last {
            format!("PR #{n} since your review")
        } else if self.local {
            format!("PR #{n} + working tree")
        } else {
            format!("PR #{n}")
        }
    }
}

/// A plugin run that isn't a `check`, a `list` or a dry run; one at a
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Job {
    Prepare(u64),
    Pull,
    Post,
    /// `post --submit-only`, after a post that left this many drafts out.
    SubmitOnly(usize),
}

impl Job {
    /// Sends to the host, so nothing may stop it halfway.
    fn posts(self) -> bool {
        matches!(self, Self::Post | Self::SubmitOnly(_))
    }
}

pub(super) struct Running {
    pub id: u64,
    pub job: Job,
    pub progress: Option<String>,
    /// A `prepare` of the PR `detect` found for the branch checked out.
    pub local: bool,
    /// A post's drafts not posted, and its last line: comments posted,
    /// comments not, and whether the review is still pending.
    pub unposted: Vec<Unposted>,
    pub posted: Option<(usize, usize, bool)>,
}

impl Running {
    fn new(id: u64, job: Job) -> Self {
        Self {
            id,
            job,
            progress: None,
            local: false,
            unposted: Vec::new(),
            posted: None,
        }
    }
}

/// A draft `post` can't post or didn't, with why.
#[derive(Debug, Clone)]
pub(super) struct Unposted {
    id: String,
    path: String,
    why: String,
}

impl Unposted {
    /// The draft of a draft line, when it's left out.
    fn of(draft: Draft) -> Option<Self> {
        match draft.plan {
            Plan::Skip(why) => Some(Self {
                id: draft.id,
                path: draft.path,
                why,
            }),
            Plan::Line | Plan::File => None,
        }
    }
}

/// A review's event, as the dialog lists them.
const EVENTS: [(char, &str, &str); 4] = [
    ('p', "pending", "pending: on the host, for you only"),
    ('c', "comment", "comment"),
    ('a', "approve", "approve"),
    ('r', "request_changes", "request changes"),
];

/// What a past event reads as, for the status bar.
fn done_as(event: &str) -> &'static str {
    match event {
        "approve" => "approved",
        "request_changes" => "changes requested",
        _ => "commented",
    }
}

/// How a post ended, in the words the status bar keeps: `review sent`,
/// `approved · 2 comments not posted`, `review still pending · 2 not
/// posted`.
fn outcome(event: &str, failed: usize, pending_review: bool) -> String {
    match (event, pending_review, failed) {
        ("pending", false, 0) => "nothing posted".to_owned(),
        ("pending", _, 0) | (_, true, 0) => "review pending".to_owned(),
        ("pending", ..) | (_, true, _) => format!("review still pending · {failed} not posted"),
        (_, false, 0) => "review sent".to_owned(),
        (event, false, n) => format!("{} · {} not posted", done_as(event), plural(n, "comment")),
    }
}

/// The dialog `P` opens.
pub(super) struct Dialog {
    event: usize,
    summary: TextBox,
    /// Keys go to the summary.
    writing: bool,
    preflight: Preflight,
}

/// What `post --dry-run`, run as the dialog opens, says.
struct Preflight {
    id: u64,
    done: bool,
    /// Drafts on a line, on the file, and left out.
    planned: Option<(usize, usize, usize)>,
    skipped: Vec<Unposted>,
    failed: Option<String>,
}

/// The prompt after a post that left drafts out and so didn't submit the
/// event picked.
pub(super) struct Held {
    failed: usize,
    unposted: Vec<Unposted>,
}

/// The PRs `list` gave, grouped by why, to open one.
pub(super) struct Picker {
    prs: Vec<Pr>,
    cursor: usize,
    scroll: usize,
}

impl Picker {
    fn new(mut prs: Vec<Pr>) -> Self {
        prs.sort_by_key(|p| p.reason);
        Self {
            prs,
            cursor: 0,
            scroll: 0,
        }
    }
}

fn group_title(reason: Reason) -> &'static str {
    match reason {
        Reason::CurrentBranch => "This branch",
        Reason::ReviewRequested => "Waiting on your review",
        Reason::Mine => "Yours",
        Reason::Other => "Other",
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

impl App {
    fn next_run(&mut self) -> u64 {
        self.run_id += 1;
        self.run_id
    }

    /// Starts on `session`, whose `prepare` goes on in the background and
    /// says the rest under the returned run id.
    pub(crate) fn start_pr(&mut self, plugin: String, session: Session) -> u64 {
        let id = self.next_run();
        self.job = Some(Running::new(id, Job::Prepare(session.number)));
        self.review = Some(plugin.clone());
        let mode = Mode::new(plugin, session);
        self.description = mode.description();
        self.pr = Some(mode);
        id
    }

    /// Opens the picker on `prs`, for `dffy pr` on a branch without one.
    pub(crate) fn pick_pr(&mut self, plugin: String, prs: Vec<Pr>) {
        self.review = Some(plugin);
        self.open_picker_of(prs);
    }

    fn open_picker_of(&mut self, prs: Vec<Pr>) {
        if prs.is_empty() {
            self.message(
                "no open pull requests for you here: none on this branch, yours or waiting on you",
            );
            return;
        }
        self.prs = Some(Picker::new(prs));
    }

    /// The run of `job` with `args`, unless a post is still running: a
    /// post can't be stopped halfway, and anything else would stop it.
    fn run(&mut self, plugin: String, job: Job, args: Vec<String>) -> Option<Action> {
        if self.job.as_ref().is_some_and(|r| r.job.posts()) {
            self.message("the review is still being posted; wait for it to finish");
            return None;
        }
        let id = self.next_run();
        self.job = Some(Running::new(id, job));
        let comments = match job {
            Job::Prepare(_) => None,
            Job::Pull | Job::Post | Job::SubmitOnly(_) => {
                self.pr.as_ref().map(|p| p.session.comments.clone())
            }
        };
        Some(Action::Plugin {
            id,
            plugin,
            args,
            comments,
            summary: None,
        })
    }

    /// The command panel's "Open pull request…": lists them to pick from,
    /// beside the run going on, which a look at the list shouldn't stop.
    pub(super) fn open_pr_picker(&mut self) -> Option<Action> {
        let plugin = self.review_plugin()?;
        self.message("listing pull requests…");
        let id = self.next_run();
        self.listing = Some(id);
        Some(Action::Plugin {
            id,
            plugin,
            args: vec!["list".to_owned()],
            comments: None,
            summary: None,
        })
    }

    /// Asks `plugin` whether the branch checked out has a PR, for plain
    /// `dffy`; the viewer goes on meanwhile (see [`App::detected`]).
    pub(crate) fn detect_pr(&mut self, plugin: String) -> Action {
        let id = self.next_run();
        self.detecting = Some(id);
        self.review = Some(plugin.clone());
        Action::Plugin {
            id,
            plugin,
            args: vec!["detect".to_owned()],
            comments: None,
            summary: None,
        }
    }

    /// What `detect` said: a PR for the branch is prepared, then shown
    /// against the working tree, unless another PR was opened meanwhile.
    fn detected(&mut self, said: Said) -> Option<Action> {
        match said {
            Said::Detected(Some(found)) if self.pr.is_none() && self.job.is_none() => {
                let plugin = self.review.clone()?;
                let n = found.number;
                let action = self.run(
                    plugin,
                    Job::Prepare(n),
                    vec!["prepare".to_owned(), n.to_string()],
                );
                if let Some(running) = &mut self.job {
                    running.local = true;
                }
                action
            }
            Said::Done(result) => {
                self.detecting = None;
                if let Err(e) = result {
                    let e = escape(e.as_bytes());
                    self.message(format!("looking for this branch's PR failed: {e}"));
                }
                None
            }
            _ => None,
        }
    }

    /// The review plugin set, or `None` after saying it isn't.
    fn review_plugin(&mut self) -> Option<String> {
        if self.piped {
            self.message("a piped diff can't be reviewed");
            return None;
        }
        let plugin = self.review.clone();
        if plugin.is_none() {
            self.message("no review plugin set: review = <name> in the config, or --review <name>");
        }
        plugin
    }

    /// `p`: fetches the PR's head and threads, and shows the new head in
    /// place.
    pub(super) fn update_pr(&mut self) -> Option<Action> {
        let Some(pr) = &self.pr else {
            self.message("not on a pull request; dffy pr <number> opens one");
            return None;
        };
        let (plugin, n) = (pr.plugin.clone(), pr.session.number);
        if self.job.as_ref().is_some_and(|r| r.job == Job::Pull) {
            self.message(format!("already updating PR #{n}"));
            return None;
        }
        self.message(format!("updating PR #{n}…"));
        self.run(plugin, Job::Pull, vec!["pull".to_owned(), n.to_string()])
    }

    /// `P`: the dialog that posts the review, and the dry run that says
    /// what posting would do, beside whatever else runs.
    pub(super) fn open_post(&mut self) -> Option<Action> {
        let Some(pr) = &self.pr else {
            self.message("not on a pull request; dffy pr <number> opens one");
            return None;
        };
        if let Some(running) = self.job.as_ref().filter(|r| r.job.posts()) {
            let at = running
                .progress
                .as_deref()
                .map(|p| format!(" {p}"))
                .unwrap_or_default();
            self.message(format!("posting{at}…"));
            return None;
        }
        let summary = TextBox::new(&pr.summary);
        let (plugin, n, comments) = (
            pr.plugin.clone(),
            pr.session.number,
            pr.session.comments.clone(),
        );
        let id = self.next_run();
        self.posting = Some(Dialog {
            event: 0,
            summary,
            writing: false,
            preflight: Preflight {
                id,
                done: false,
                planned: None,
                skipped: Vec::new(),
                failed: None,
            },
        });
        Some(Action::Plugin {
            id,
            plugin,
            args: vec!["post".to_owned(), n.to_string(), "--dry-run".to_owned()],
            comments: Some(comments),
            summary: None,
        })
    }

    /// What the dialog's dry run said.
    fn preflighted(&mut self, said: Said) {
        let Some(preflight) = self.posting.as_mut().map(|d| &mut d.preflight) else {
            return;
        };
        match said {
            Said::Draft(draft) => preflight.skipped.extend(Unposted::of(draft)),
            Said::Planned { line, file, skip } => preflight.planned = Some((line, file, skip)),
            Said::Done(result) => {
                preflight.done = true;
                preflight.failed = result.err();
            }
            _ => {}
        }
        self.dirty = true;
    }

    /// Focuses the thread of the first of `drafts` still in the file, and
    /// says why it wasn't posted; `m`/`M` go on from there.
    fn go_to_unposted(&mut self, drafts: &[Unposted]) {
        let Some(comments) = &self.comments else {
            return;
        };
        let Some((draft, root)) = drafts
            .iter()
            .find_map(|d| Some((d, comments.thread_root(&d.id)?)))
        else {
            self.message("the drafts left out aren't in the file any more");
            return;
        };
        let mut note = format!("not posted: {}", escape(draft.why.as_bytes()));
        if drafts.len() > 1 {
            let _ = write!(
                note,
                " · {} more · m/M go between threads",
                drafts.len() - 1
            );
        }
        self.open_thread(&root);
        self.message(note);
    }

    /// "Since last review": the range from the head your last review was
    /// on to the head, or back to the whole PR.
    pub(super) fn toggle_since_review(&mut self) -> Option<Action> {
        let Some(pr) = &mut self.pr else {
            self.message("not on a pull request; dffy pr <number> opens one");
            return None;
        };
        let Some(last) = pr.session.last_reviewed_head.clone() else {
            self.message("no review of yours posted from dffy on this PR yet");
            return None;
        };
        if last == pr.session.head && !pr.since_last {
            self.message("nothing new since your last review");
            return None;
        }
        pr.since_last = !pr.since_last;
        let note = if pr.since_last {
            format!(
                "since your last review: {}..{}",
                short(&last),
                short(&pr.session.head)
            )
        } else {
            "the whole PR".to_owned()
        };
        self.message(note);
        self.retarget(None, None)
    }

    /// Shows the PR's range, as it stands now, and its comments when
    /// `comments` names another file.
    fn retarget(
        &mut self,
        comments: Option<PathBuf>,
        moved: Option<(String, String)>,
    ) -> Option<Action> {
        Some(Action::Retarget {
            revisions: self.pr_range()?,
            comments,
            moved,
        })
    }

    /// The PR's range as it stands now, which the title and the list of
    /// files take.
    fn pr_range(&mut self) -> Option<Revisions> {
        let pr = self.pr.as_ref()?;
        let revisions = pr.revisions();
        self.description = pr.description();
        if let Some((selected, _)) = &mut self.selected {
            selected.clone_from(&revisions);
        }
        Some(revisions)
    }

    /// The head of your last review, while "Since last review" shows from
    /// it.
    pub(crate) fn reviewed_from(&self) -> Option<&str> {
        let pr = self.pr.as_ref().filter(|p| p.since_last)?;
        pr.session.last_reviewed_head.as_deref()
    }

    /// The head of your last review isn't in the clone, so the range from
    /// it can't be listed: back to the whole PR, whose range is returned.
    pub(crate) fn reviewed_head_missing(&mut self) -> Option<Revisions> {
        let pr = self.pr.as_mut()?;
        pr.since_last = false;
        let last = pr.session.last_reviewed_head.clone().unwrap_or_default();
        self.message(format!(
            "last reviewed commit {} isn't in this clone (force-pushed away?)",
            short(&last)
        ));
        self.pr_range()
    }

    /// Whether "Since last review" is on, for the command panel.
    pub(super) fn since_review(&self) -> bool {
        self.pr.as_ref().is_some_and(|p| p.since_last)
    }

    /// The check, when it's due: none while the PR's head is being fetched.
    pub(super) fn due_check(&mut self) -> Option<Action> {
        let busy = matches!(
            self.job.as_ref().map(|r| r.job),
            Some(Job::Pull | Job::Prepare(_))
        );
        let id = self.run_id + 1;
        let pr = self.pr.as_mut()?;
        if pr.check.is_some() || Instant::now() < pr.next_check {
            return None;
        }
        if busy {
            pr.next_check = Instant::now() + CHECK_EVERY;
            return None;
        }
        pr.check = Some(id);
        self.run_id = id;
        Some(Action::Plugin {
            id,
            plugin: pr.plugin.clone(),
            args: vec!["check".to_owned(), pr.session.number.to_string()],
            comments: Some(pr.session.comments.clone()),
            summary: None,
        })
    }

    /// When the next check is due, while none runs.
    pub(super) fn check_deadline(&self) -> Option<Instant> {
        self.pr
            .as_ref()
            .filter(|p| p.check.is_none())
            .map(|p| p.next_check)
    }

    /// `x` on a thread in PR mode, for the post dialog's count.
    pub(super) fn note_resolve(&mut self, root: &str, was: bool) {
        if let Some(pr) = &mut self.pr {
            match pr.resolves.get(root) {
                Some(&before) if before != was => {
                    pr.resolves.remove(root);
                }
                Some(_) => {}
                None => {
                    pr.resolves.insert(root.to_owned(), was);
                }
            }
        }
    }

    /// What a run said. Runs no longer wanted say nothing.
    pub(super) fn review_said(&mut self, id: u64, said: Said) -> Option<Action> {
        if self.pr.as_ref().is_some_and(|p| p.check == Some(id)) {
            self.checked(said);
            return None;
        }
        if self.listing == Some(id) {
            self.listed(said);
            return None;
        }
        if self.detecting == Some(id) {
            return self.detected(said);
        }
        if self.posting.as_ref().is_some_and(|d| d.preflight.id == id) {
            self.preflighted(said);
            return None;
        }
        let running = self.job.as_mut().filter(|r| r.id == id)?;
        let (job, local) = (running.job, running.local);
        self.dirty = true;
        match said {
            Said::Progress(progress) => running.progress = Some(progress),
            Said::Session(session) => return self.prepared(job, local, session),
            Said::Draft(draft) => running.unposted.extend(Unposted::of(draft)),
            Said::Posted {
                posted,
                failed,
                pending_review,
                url,
            } => {
                running.posted = Some((posted, failed, pending_review));
                self.link = url.filter(|u| is_web(u));
            }
            Said::Done(result) => {
                let running = self.job.take()?;
                match (job, result) {
                    (Job::Post, Ok(())) => self.posted(running),
                    (Job::SubmitOnly(failed), Ok(())) => self.submitted(failed),
                    (_, Ok(())) => {}
                    (job, Err(e)) => {
                        let e = escape(e.as_bytes());
                        self.message(match job {
                            Job::Prepare(n) => format!("PR #{n}: {e}"),
                            Job::Pull => format!("updating the PR failed: {e}"),
                            Job::Post => format!("posting failed: {e}"),
                            Job::SubmitOnly(_) => {
                                format!("submitting the review failed, it's still pending: {e}")
                            }
                        });
                    }
                }
            }
            Said::Prs(_) | Said::Head { .. } | Said::Detected(_) | Said::Planned { .. } => {}
        }
        None
    }

    /// A post that finished: what it did goes in the status bar, unless the
    /// host refused drafts and so the event picked wasn't submitted, which
    /// asks first. Drafts the dialog showed couldn't be posted don't hold
    /// the event back.
    fn posted(&mut self, running: Running) {
        let Some(pr) = &mut self.pr else {
            return;
        };
        let (posted, failed, pending_review) = running.posted.unwrap_or_default();
        let event = EVENTS[pr.event].1;
        // As the plugin records it: a pending review with nothing in it
        // reviewed nothing.
        if posted > 0 || (event != "pending" && !pending_review) {
            pr.session.last_reviewed_head = Some(pr.session.head.clone());
        }
        pr.resolves.clear();
        let text = outcome(event, failed, pending_review);
        if event != "pending" && pending_review && failed > 0 {
            pr.outcome = Some(text);
            self.held = Some(Held {
                failed,
                unposted: running.unposted,
            });
            self.message = None;
            return;
        }
        pr.summary.clear();
        pr.outcome = Some(text.clone());
        // The link goes in the note only: it would crowd the status bar
        // out of room for the PR's part.
        match self.link.as_deref().filter(|_| failed == 0) {
            Some(url) => self.message(format!("{text} · {url}")),
            None => self.message = None,
        }
    }

    /// `post --submit-only` finished: the review went without the drafts
    /// left out.
    fn submitted(&mut self, failed: usize) {
        let Some(pr) = &mut self.pr else {
            return;
        };
        pr.session.last_reviewed_head = Some(pr.session.head.clone());
        pr.summary.clear();
        pr.outcome = Some(outcome(EVENTS[pr.event].1, failed, false));
        self.message = None;
    }

    /// Another overlay or a text input takes the keys. The prompt after a
    /// post, which opens without a key, waits until it's closed, so an `s`
    /// typed there isn't taken for submitting.
    pub(super) fn keys_elsewhere(&self) -> bool {
        self.commands.is_some()
            || self.help.is_some()
            || self.panel.is_some()
            || self.picker.is_some()
            || self.tree.editing
            || self.search.as_ref().is_some_and(|s| s.editing)
    }

    /// Input while the prompt after a post that left drafts out is open.
    pub(super) fn held_input(&mut self, input: &Input) -> Option<Action> {
        let &Input::Key(key, mods) = input else {
            return None;
        };
        if key == Key::Char('c') && mods.contains(Mods::CTRL) {
            return Some(Action::Quit);
        }
        let failed = self.held.as_ref()?.failed;
        match key {
            Key::Char('s') => {
                self.held = None;
                let pr = self.pr.as_ref()?;
                let (plugin, n, summary) =
                    (pr.plugin.clone(), pr.session.number, pr.summary.clone());
                let event = EVENTS[pr.event].1;
                self.message("submitting the review without them…");
                let mut action = self.run(
                    plugin,
                    Job::SubmitOnly(failed),
                    vec![
                        "post".to_owned(),
                        n.to_string(),
                        "--submit-only".to_owned(),
                        "--event".to_owned(),
                        event.to_owned(),
                    ],
                );
                if let Some(Action::Plugin { summary: s, .. }) = &mut action {
                    *s = (!summary.is_empty()).then_some(summary);
                }
                return action;
            }
            Key::Char('f') => {
                if let Some(held) = self.held.take() {
                    self.go_to_unposted(&held.unposted);
                }
            }
            Key::Char('k') | Key::Esc => self.held = None,
            _ => {}
        }
        None
    }

    /// What `list` said, for the picker.
    fn listed(&mut self, said: Said) {
        match said {
            Said::Prs(prs) => {
                self.message = None;
                self.open_picker_of(prs);
            }
            Said::Done(result) => {
                self.listing = None;
                if let Err(e) = result {
                    let e = escape(e.as_bytes());
                    self.message(format!("listing pull requests failed: {e}"));
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    /// A session line: a PR opened from the picker or found for the
    /// branch (`local`), or the PR updated.
    fn prepared(&mut self, job: Job, local: bool, session: Session) -> Option<Action> {
        let plugin = self.review.clone().unwrap_or_default();
        match job {
            Job::Prepare(_) => {
                let comments = session.comments.clone();
                let title = session.title.clone().unwrap_or_default();
                let n = session.number;
                let mut mode = Mode::new(plugin, session);
                mode.local = local;
                self.pr = Some(mode);
                let on = if local { " · this branch's" } else { "" };
                self.message(format!("PR #{n}{on} · {}", escape(title.as_bytes())));
                self.retarget(Some(comments), None)
            }
            Job::Pull => {
                let pr = self.pr.as_mut()?;
                pr.moved = None;
                let old = std::mem::replace(&mut pr.session, session);
                let s = &pr.session;
                let n = s.number;
                let comments = (s.comments != old.comments).then(|| s.comments.clone());
                if (&s.base, &s.head) == (&old.base, &old.head) && comments.is_none() {
                    self.message(format!("PR #{n} is up to date"));
                    return None;
                }
                let moved = (s.head != old.head).then(|| (old.head.clone(), s.head.clone()));
                self.message(format!("PR #{n} updated"));
                self.retarget(comments, moved)
            }
            Job::Post | Job::SubmitOnly(_) => None,
        }
    }

    /// After an update moved the head from `old` to `new`, how many
    /// commits that is, when git could tell.
    pub(crate) fn pr_updated(&mut self, count: Option<usize>) {
        let (Some(pr), Some(count)) = (&self.pr, count) else {
            return;
        };
        let n = pr.session.number;
        let s = if count == 1 { "" } else { "s" };
        self.message(format!("PR #{n} updated · {count} new commit{s}"));
    }

    fn checked(&mut self, said: Said) {
        let Some(pr) = &mut self.pr else {
            return;
        };
        match said {
            Said::Head { head, moved } => {
                pr.moved = (moved && head != pr.session.head).then_some(head);
                self.dirty = true;
            }
            Said::Done(result) => {
                pr.check = None;
                pr.next_check = Instant::now() + CHECK_EVERY;
                if let Err(e) = result {
                    let e = escape(e.as_bytes());
                    self.message(format!("checking the PR failed: {e}"));
                }
            }
            _ => {}
        }
    }

    /// The PR's part of the status bar: `PR #7`, what's running, and
    /// whether the head moved.
    pub(super) fn pr_status(&self) -> Option<String> {
        let pr = self.pr.as_ref()?;
        let mut text = format!("PR #{}", pr.session.number);
        if let Some(running) = &self.job {
            let doing = match running.job {
                Job::Prepare(_) => "fetching threads",
                Job::Pull => "updating",
                Job::Post => "posting",
                Job::SubmitOnly(_) => "submitting",
            };
            let _ = write!(text, " · {doing}");
            if let Some(progress) = &running.progress {
                let _ = write!(text, " {progress}");
            }
        } else if pr.moved.is_some() {
            text.push_str(" updated · new commits · p to update");
        }
        if let Some(outcome) = &pr.outcome {
            let _ = write!(text, " · {outcome}");
        }
        if self.link.is_some() {
            text.push_str(" · o opens it");
        }
        Some(text)
    }

    /// Input that PR mode's overlays take, or `o` while a posted review's
    /// note shows; `Continue` when it's for the rest of the viewer.
    pub(super) fn pr_input(&mut self, input: &Input) -> ControlFlow<Option<Action>> {
        if self.posting.is_some() {
            return ControlFlow::Break(self.post_input(input));
        }
        if self.held.is_some() && !self.keys_elsewhere() {
            return ControlFlow::Break(self.held_input(input));
        }
        if self.prs.is_some() {
            return ControlFlow::Break(self.pr_picker_input(input));
        }
        match (&self.link, input) {
            (Some(url), Input::Key(Key::Char('o'), Mods::NONE)) => {
                ControlFlow::Break(Some(Action::Browse(url.clone())))
            }
            (Some(_), Input::Key(..)) => {
                self.link = None;
                ControlFlow::Continue(())
            }
            _ => ControlFlow::Continue(()),
        }
    }

    /// Input while the post dialog is open.
    pub(super) fn post_input(&mut self, input: &Input) -> Option<Action> {
        let dialog = self.posting.as_mut()?;
        let (key, mods) = match input {
            Input::Paste(text) => {
                dialog.writing = true;
                dialog.summary.paste(text);
                return None;
            }
            Input::Key(key, mods) => (*key, *mods),
            _ => return None,
        };
        if key == Key::Char('c') && mods.contains(Mods::CTRL) {
            return Some(Action::Quit);
        }
        if dialog.writing {
            if key == Key::Esc {
                dialog.writing = false;
            } else {
                dialog.summary.key(key, mods);
            }
            return None;
        }
        match key {
            Key::Esc => {
                if let (Some(pr), Some(dialog)) = (&mut self.pr, self.posting.take()) {
                    dialog.summary.text().clone_into(&mut pr.summary);
                }
            }
            Key::Tab | Key::Char('s') => dialog.writing = true,
            Key::Up | Key::Char('k') => dialog.event = dialog.event.saturating_sub(1),
            Key::Down | Key::Char('j') => dialog.event = (dialog.event + 1).min(EVENTS.len() - 1),
            Key::Char(c) if EVENTS.iter().any(|e| e.0 == c) => {
                dialog.event = EVENTS.iter().position(|e| e.0 == c).unwrap_or(0);
            }
            Key::Char('v') if dialog.preflight.skipped.is_empty() => {
                let p = &dialog.preflight;
                let note = match p.planned {
                    Some(_) => "every draft can be posted",
                    None if p.done => "couldn't check what can be posted",
                    None => "still checking what can be posted",
                };
                self.message(note);
            }
            Key::Char('v') => {
                let skipped = std::mem::take(&mut dialog.preflight.skipped);
                if let (Some(pr), Some(dialog)) = (&mut self.pr, self.posting.take()) {
                    dialog.summary.text().clone_into(&mut pr.summary);
                }
                self.go_to_unposted(&skipped);
            }
            Key::Enter => return self.send_review(),
            _ => {}
        }
        None
    }

    /// Posts the review as the dialog has it.
    fn send_review(&mut self) -> Option<Action> {
        let dialog = self.posting.take()?;
        let pr = self.pr.as_mut()?;
        let summary = dialog.summary.text().trim_end().to_owned();
        pr.summary.clone_from(&summary);
        let (plugin, n) = (pr.plugin.clone(), pr.session.number);
        pr.event = dialog.event;
        pr.outcome = None;
        let event = EVENTS[dialog.event].1;
        self.message(format!("posting the review on PR #{n}…"));
        let mut action = self.run(
            plugin,
            Job::Post,
            vec![
                "post".to_owned(),
                n.to_string(),
                "--event".to_owned(),
                event.to_owned(),
            ],
        );
        if let Some(Action::Plugin { summary: s, .. }) = &mut action {
            *s = (!summary.is_empty()).then_some(summary);
        }
        action
    }

    /// What `P` would send: new comments, replies and resolve changes.
    pub(super) fn unsent(&self) -> (usize, usize, usize) {
        let (comments, replies) = self.comments.as_ref().map_or((0, 0), Comments::unsent);
        let resolves = self.pr.as_ref().map_or(0, |p| p.resolves.len());
        (comments, replies, resolves)
    }
}

impl Preflight {
    /// What goes: the plugin's count of drafts once its dry run is in, in
    /// which replies are comments, else dffy's own of `unsent`.
    fn what(&self, (comments, replies, resolves): (usize, usize, usize)) -> String {
        let mut parts = Vec::new();
        if let Some((line, file, skip)) = self.planned {
            if line + file + skip > 0 {
                parts.push(plural(line + file + skip, "comment"));
            }
            if file > 0 {
                parts.push(format!("{file} as a file comment"));
            }
        } else {
            if comments > 0 {
                parts.push(plural(comments, "comment"));
            }
            if replies > 0 {
                parts.push(plural(replies, "reply").replace("replys", "replies"));
            }
        }
        if resolves > 0 {
            parts.push(plural(resolves, "resolve change"));
        }
        if parts.is_empty() {
            "no comments to send".to_owned()
        } else {
            parts.join(" · ")
        }
    }

    /// The line under it: the drafts that can't be posted, which sending
    /// leaves as drafts, or how the check goes. The first is a warning.
    fn note(&self) -> Option<(String, bool)> {
        match (self.planned, &self.failed) {
            (Some((.., skip)), _) => (skip > 0).then(|| {
                let stay = if skip == 1 {
                    "stays a draft"
                } else {
                    "stay drafts"
                };
                (
                    format!("{skip} can't be posted and {stay} · v goes to them"),
                    true,
                )
            }),
            (None, Some(e)) => Some((
                format!(
                    "couldn't check what can be posted: {}",
                    escape(e.as_bytes())
                ),
                false,
            )),
            (None, None) => {
                (!self.done).then(|| ("checking what can be posted…".to_owned(), false))
            }
        }
    }
}

impl Dialog {
    /// Draws the dialog for `pr`, which would send `unsent` (see
    /// [`App::unsent`]).
    pub(super) fn draw(
        &mut self,
        grid: &mut Grid,
        theme: &Theme,
        pr: &Mode,
        (comments, replies, resolves): (usize, usize, usize),
    ) {
        let dialog = self;
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 20 || gh < 8 {
            return;
        }
        let width = if gw < help::FULL_SCREEN_BELOW {
            gw
        } else {
            64.min(gw - 4)
        };
        let inner = width - 4;
        let mut lines: Vec<(String, Style)> = Vec::new();
        let dim = theme.style(Role::Dim);
        lines.push((
            dialog.preflight.what((comments, replies, resolves)),
            Style::PLAIN,
        ));
        lines.extend(
            dialog
                .preflight
                .note()
                .map(|(text, warn)| (text, if warn { theme.style(Role::Warn) } else { dim })),
        );
        if pr.moved.is_some() {
            lines.push((
                "! the PR has new commits; this review goes on the head shown".to_owned(),
                theme.style(Role::Warn),
            ));
        }
        lines.push((String::new(), Style::PLAIN));
        let first_event = lines.len();
        for (i, (key, _, label)) in EVENTS.iter().enumerate() {
            let dot = if i == dialog.event { "(•)" } else { "( )" };
            lines.push((format!("{dot} {key}  {label}"), Style::PLAIN));
        }
        lines.push((String::new(), Style::PLAIN));
        let hint = if dialog.writing {
            "summary · esc back"
        } else {
            "summary · s writes"
        };
        lines.push((hint.to_owned(), dim));
        let summary_at = lines.len();
        let rows = dialog.summary.rows(inner.saturating_sub(2));
        let shown = rows.len().clamp(1, 6);
        let skip = rows
            .iter()
            .position(|(_, c)| c.is_some())
            .map_or(0, |at| (at + 1).saturating_sub(shown));
        let footer = if dialog.writing {
            "enter new line · esc done"
        } else {
            "enter sends · j/k or the letter picks · esc closes"
        };
        let height = (lines.len() + shown + 4).min(gh);
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        let title = format!(" post review · PR #{} ", pr.session.number);
        help::frame(grid, (x0, y0, width, height), &title, theme);
        let (x, right) = (x0 + 2, x0 + width - 2);
        for (i, (text, style)) in lines.iter().enumerate() {
            let y = y0 + 1 + i;
            if y + 2 >= y0 + height {
                break;
            }
            grid.put_str(x, y, right, text, *style);
            let event = i.checked_sub(first_event).filter(|&e| e < EVENTS.len());
            if event == Some(dialog.event) {
                let mut bold = theme.selected(Style::PLAIN, !dialog.writing);
                bold.attributes |= Attributes::BOLD;
                grid.map_styles(x, y, right, |_| bold);
            }
        }
        for (i, (text, cursor)) in rows.iter().skip(skip).take(shown).enumerate() {
            let y = y0 + 1 + summary_at + i;
            if y + 2 >= y0 + height {
                break;
            }
            let end = grid.put_str(x + 2, y, right, text, Style::PLAIN);
            if let (Some(at), true) = (cursor, dialog.writing) {
                let column = x + 2 + text[..*at].width();
                grid.map_styles(column, y, (column + 1).max(end.min(column + 1)), |s| {
                    theme.selected(s, true)
                });
                if column >= end {
                    grid.put_str(column, y, right, " ", theme.selected(Style::PLAIN, true));
                }
            }
        }
        grid.put_str(x, y0 + height - 2, right, footer, dim);
    }
}

impl Held {
    pub(super) fn draw(&self, grid: &mut Grid, theme: &Theme, pr: &Mode) {
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 20 || gh < 8 {
            return;
        }
        let width = if gw < help::FULL_SCREEN_BELOW {
            gw
        } else {
            64.min(gw - 4)
        };
        let dim = theme.style(Role::Dim);
        let n = self.failed;
        let were = if n == 1 { "wasn't" } else { "weren't" };
        let mut lines = vec![(
            format!("{} {were} posted:", plural(n, "comment")),
            Style::PLAIN,
        )];
        // Room for the rest of the prompt on a phone's screen.
        let room = gh.saturating_sub(12).max(1);
        for draft in self.unposted.iter().take(room) {
            // The reason first: a deep path would push it off the line.
            let name = draft.path.rsplit('/').next().unwrap_or_default();
            lines.push((
                format!(
                    "· {} · {}",
                    escape(draft.why.as_bytes()),
                    escape(name.as_bytes())
                ),
                dim,
            ));
        }
        if self.unposted.len() > room {
            lines.push((format!("· {} more", self.unposted.len() - room), dim));
        }
        lines.extend([
            (String::new(), Style::PLAIN),
            (
                "so the review wasn't submitted, and is still pending".to_owned(),
                theme.style(Role::Warn),
            ),
            (String::new(), Style::PLAIN),
            ("s  submit it without them".to_owned(), Style::PLAIN),
            (
                "f  fix them: go to the first, the review stays pending".to_owned(),
                Style::PLAIN,
            ),
            ("k  keep the review pending".to_owned(), Style::PLAIN),
        ]);
        let height = (lines.len() + 4).min(gh);
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        let title = format!(" review not sent · PR #{} ", pr.session.number);
        help::frame(grid, (x0, y0, width, height), &title, theme);
        let (x, right) = (x0 + 2, x0 + width - 2);
        for (i, (text, style)) in lines.iter().enumerate() {
            let y = y0 + 1 + i;
            if y + 2 >= y0 + height {
                break;
            }
            grid.put_str(x, y, right, text, *style);
        }
        grid.put_str(
            x,
            y0 + height - 2,
            right,
            "s, f or k · esc keeps it pending",
            dim,
        );
    }
}

impl App {
    /// Input while the PR picker is open: `j`/`k` or the arrows move,
    /// `enter` opens, `esc` closes.
    pub(super) fn pr_picker_input(&mut self, input: &Input) -> Option<Action> {
        let picker = self.prs.as_mut()?;
        let last = picker.prs.len().saturating_sub(1);
        match *input {
            Input::Key(Key::Char('c'), mods) if mods.contains(Mods::CTRL) => {
                return Some(Action::Quit);
            }
            Input::Key(Key::Esc | Key::Char('q'), _) => self.prs = None,
            Input::Key(Key::Up | Key::Char('k'), _) => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            Input::Key(Key::Down | Key::Char('j'), _) => {
                picker.cursor = (picker.cursor + 1).min(last);
            }
            Input::Scroll { delta, .. } => {
                picker.cursor = picker
                    .cursor
                    .saturating_add_signed(isize::from(delta))
                    .min(last);
            }
            Input::Key(Key::Enter, _) => {
                let n = picker.prs.get(picker.cursor)?.number;
                self.prs = None;
                return self.open_pr(n);
            }
            _ => {}
        }
        None
    }

    fn open_pr(&mut self, n: u64) -> Option<Action> {
        if self.pr.as_ref().is_some_and(|p| p.session.number == n) {
            self.message(format!("PR #{n} is open"));
            return None;
        }
        let plugin = self.review_plugin()?;
        self.message(format!("fetching #{n}…"));
        self.run(
            plugin,
            Job::Prepare(n),
            vec!["prepare".to_owned(), n.to_string()],
        )
    }
}

impl Picker {
    pub(super) fn draw(&mut self, grid: &mut Grid, theme: &Theme) {
        let picker = self;
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 20 || gh < 6 {
            return;
        }
        // Group titles, then each PR under its group's.
        let mut lines: Vec<Option<usize>> = Vec::new();
        let mut titles: Vec<&'static str> = Vec::new();
        let mut last = None;
        for (i, pr) in picker.prs.iter().enumerate() {
            if last != Some(pr.reason) {
                if last.is_some() {
                    lines.push(None);
                    titles.push("");
                }
                lines.push(None);
                titles.push(group_title(pr.reason));
                last = Some(pr.reason);
            }
            lines.push(Some(i));
            titles.push("");
        }
        let full = gw < help::FULL_SCREEN_BELOW;
        let width = if full { gw } else { 80.min(gw - 4) };
        let height = if full {
            gh
        } else {
            (lines.len() + 4).min(gh - 2)
        };
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        help::frame(grid, (x0, y0, width, height), " pull requests ", theme);
        let rows = height.saturating_sub(3);
        let at = lines
            .iter()
            .position(|l| *l == Some(picker.cursor))
            .unwrap_or(0);
        if at < picker.scroll {
            picker.scroll = at.saturating_sub(1);
        } else if at >= picker.scroll + rows {
            picker.scroll = at + 1 - rows;
        }
        let (x, right) = (x0 + 2, x0 + width - 3);
        let dim = theme.style(Role::Dim);
        for (row, (line, title)) in lines
            .iter()
            .zip(&titles)
            .enumerate()
            .skip(picker.scroll)
            .take(rows)
        {
            let y = y0 + 1 + row - picker.scroll;
            let Some(i) = *line else {
                let mut bold = theme.style(Role::Plain);
                bold.attributes |= Attributes::BOLD;
                grid.put_str(x, y, right, title, bold);
                continue;
            };
            let pr = &picker.prs[i];
            let selected = i == picker.cursor;
            let mark = |s: Style| if selected { theme.selected(s, true) } else { s };
            if selected {
                grid.fill(x - 1, y, right + 1, mark(Style::PLAIN));
            }
            let after = grid.put_str(
                x,
                y,
                right,
                &format!("#{:<6}", pr.number),
                mark(theme.style(Role::Hint)),
            );
            let author = format!("  {}", escape(pr.author.as_bytes()));
            let title_end = right.saturating_sub(author.width());
            grid.put_str(
                after + 1,
                y,
                title_end,
                &escape(pr.title.as_bytes()),
                mark(Style::PLAIN),
            );
            grid.put_str(title_end, y, right, &author, mark(dim));
        }
        help::scrollbar(
            grid,
            (x0 + width - 2, y0 + 1, rows),
            (lines.len(), picker.scroll),
            dim,
        );
        grid.put_str(
            x,
            y0 + height - 2,
            right,
            "enter opens · j/k move · esc close",
            dim,
        );
    }
}
