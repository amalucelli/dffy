//! Review mode: the viewer on a review that a review plugin prepared,
//! showing `<base>...<head>` with the review's comments file. The plugin
//! runs in the background (see [`crate::plugin::review`]): what it says
//! comes back as [`Event::Review`] for the run's id, and runs that are no
//! longer wanted are ignored. Here are the state of the review, the run
//! going on, the picker of reviews and the dialog that posts the review.
//! Every word naming the host's kind of review is the plugin's.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dffy_core::git::Revisions;
use dffy_core::view::Attributes;
use unicode_width::UnicodeWidthStr;

use std::fmt::Write as _;

use super::{Action, App, Comments, escape, is_web};
use crate::plugin::review::{self, Draft, Listed, Plan, Reason, ReviewEvent, Said, Session};
use crate::term::{Grid, Input, Key, Mods};
use crate::theme::{Role, Style, Theme};
use crate::tui::help;
use crate::tui::textbox::TextBox;

/// How often the plugin is asked whether the review's head moved.
pub(super) const CHECK_EVERY: Duration = Duration::from_secs(60);

/// The review shown.
pub(super) struct Mode {
    pub plugin: String,
    pub session: Session,
    /// The session as titles and messages name it: `PR #7`.
    pub label: String,
    /// The range is `<last_reviewed_head>..<head>`, not the whole review.
    pub since_last: bool,
    /// Found as the branch checked out by plain `dffy`: the range and title
    /// it showed, which stay, the review adding its comments and status.
    pub plain: Option<(Revisions, String)>,
    /// With `plain`: the review's base against the working tree instead,
    /// so changes not pushed yet show with the review's.
    pub whole: bool,
    /// The event of the last post.
    pub event: Option<ReviewEvent>,
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
    fn new(plugin: String, session: Session, label: String) -> Self {
        Self {
            plugin,
            session,
            label,
            since_last: false,
            plain: None,
            whole: false,
            event: None,
            outcome: None,
            moved: None,
            check: None,
            next_check: Instant::now() + CHECK_EVERY,
            resolves: HashMap::new(),
            summary: String::new(),
        }
    }

    /// The revisions shown: the review, or what changed since the last one
    /// you posted.
    fn revisions(&self) -> Revisions {
        let s = &self.session;
        let range = match (&s.last_reviewed_head, self.since_last, &self.plain) {
            (Some(last), true, _) => format!("{last}..{}", s.head),
            (.., Some((plain, _))) if !self.whole => return plain.clone(),
            (.., Some(_)) => {
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
        let label = &self.label;
        match &self.plain {
            _ if self.since_last => format!("{label} since your review"),
            Some((_, plain)) if !self.whole => plain.clone(),
            Some(_) => format!("{label} + working tree"),
            None => label.clone(),
        }
    }
}

/// A plugin run that isn't a `check`, a `list` or a dry run; one at a
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Job {
    Prepare,
    Pull,
    Post,
    /// `post --apply-event`, after a post that left this many drafts out.
    ApplyEvent(usize),
}

impl Job {
    /// Sends to the host, so nothing may stop it halfway.
    fn posts(self) -> bool {
        matches!(self, Self::Post | Self::ApplyEvent(_))
    }
}

pub(super) struct Running {
    pub id: u64,
    pub job: Job,
    /// The review it's of, as messages name it.
    pub label: String,
    pub progress: Option<String>,
    /// A `prepare` of the review `detect` found for the branch checked out.
    pub local: bool,
    /// A post's drafts not posted, and its last line: comments posted,
    /// comments not, and whether the event wasn't applied, with the
    /// plugin's words for that.
    pub unposted: Vec<Unposted>,
    pub posted: Option<(usize, usize, bool)>,
    pub held_message: Option<String>,
}

impl Running {
    fn new(id: u64, job: Job, label: String) -> Self {
        Self {
            id,
            job,
            label,
            progress: None,
            local: false,
            unposted: Vec::new(),
            posted: None,
            held_message: None,
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

/// How a post of `event` ended, in the words the status bar keeps:
/// `review sent`, `approved · 2 comments not posted`, the plugin's words
/// for an event `held` back (`review still pending · 2 not posted`), or
/// `nothing posted`.
fn outcome(event: &ReviewEvent, (posted, failed): (usize, usize), held: Option<&str>) -> String {
    let not_posted = |text: &str| match failed {
        0 => text.to_owned(),
        n => format!("{text} · {} not posted", plural(n, "comment")),
    };
    match held {
        Some(held) if failed > 0 => format!("{held} · {failed} not posted"),
        Some(held) => held.to_owned(),
        None if !event.submits && posted + failed == 0 => "nothing posted".to_owned(),
        None if event.submits && failed == 0 => "review sent".to_owned(),
        None => not_posted(&event.done),
    }
}

/// What the status bar says of an event held back when the plugin doesn't
/// say.
const HELD: &str = "review not sent";

/// The dialog `P` opens.
pub(super) struct Dialog {
    /// The plugin's events, and the one picked.
    events: Vec<ReviewEvent>,
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

/// The prompt after a post that left drafts out and so didn't apply the
/// event picked.
pub(super) struct Held {
    failed: usize,
    unposted: Vec<Unposted>,
    event: ReviewEvent,
    /// The plugin's words for where that leaves the review.
    message: Option<String>,
}

/// The reviews `list` gave, grouped, to open one: the reasons dffy knows
/// first, then the plugin's groups in the order they came.
pub(super) struct Picker {
    reviews: Vec<Listed>,
    /// Each review's group, as an index into `groups`.
    group: Vec<usize>,
    groups: Vec<String>,
    /// Each review's id as shown, with the plugin's prefix, and the widest.
    tags: Vec<String>,
    tag_width: usize,
    cursor: usize,
    scroll: usize,
    /// The plugin's plural noun, for the frame.
    title: String,
}

impl Picker {
    fn new(mut reviews: Vec<Listed>, nouns: &str, prefix: &str) -> Self {
        let mut groups: Vec<String> = Vec::new();
        let mut title_of = |listed: &Listed| {
            let title = match listed.reason {
                Reason::CurrentBranch => "This branch",
                Reason::ReviewRequested => "Waiting on your review",
                Reason::Mine => "Yours",
                Reason::Other => listed.group.as_deref().unwrap_or("Other"),
            };
            groups.iter().position(|g| g == title).unwrap_or_else(|| {
                groups.push(title.to_owned());
                groups.len() - 1
            })
        };
        // Sorted stably, so the plugin's groups keep the order they came.
        let mut keyed: Vec<(Reason, usize, Listed)> = (reviews.drain(..))
            .map(|listed| (listed.reason, title_of(&listed), listed))
            .collect();
        keyed.sort_by_key(|&(reason, group, _)| (reason, group));
        let (group, reviews): (Vec<usize>, Vec<Listed>) =
            keyed.into_iter().map(|(_, g, p)| (g, p)).unzip();
        let tags: Vec<String> = (reviews.iter())
            .map(|p| format!("{prefix}{}", escape(p.shown().as_bytes())))
            .collect();
        let tag_width = tags
            .iter()
            .map(|t| t.width())
            .max()
            .unwrap_or(0)
            .clamp(7, 16);
        Self {
            reviews,
            group,
            groups,
            tags,
            tag_width,
            cursor: 0,
            scroll: 0,
            title: format!(" {nouns} "),
        }
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
    pub(crate) fn start_review(&mut self, plugin: String, session: Session) -> u64 {
        let id = self.next_run();
        let label = self.review_label(session.shown());
        self.job = Some(Running::new(id, Job::Prepare, label.clone()));
        self.review = Some(plugin.clone());
        let mode = Mode::new(plugin, session, label);
        self.description = mode.description();
        self.reviewing = Some(mode);
        // The comments file shown is the session's: your comments in it
        // not posted yet are drafts.
        let changed = (self.comments.as_mut()).map_or_else(Vec::new, |c| c.set_session(true));
        if !changed.is_empty() {
            self.placed_again(&changed);
        }
        id
    }

    /// Opens the picker on `reviews`, for the review command (`dffy pr`) on a
    /// branch without one.
    pub(crate) fn pick_review(&mut self, plugin: String, reviews: Vec<Listed>) {
        self.review = Some(plugin);
        self.open_picker_of(reviews);
    }

    fn open_picker_of(&mut self, reviews: Vec<Listed>) {
        if reviews.is_empty() {
            self.message(format!("no {} listed for you here", self.noun(true)));
            return;
        }
        let prefix = self.naming.as_ref().map_or("", |n| n.id_prefix.as_str());
        self.reviews = Some(Picker::new(reviews, self.noun(true), prefix));
    }

    /// The review plugin's word for what it reviews, or the plural;
    /// `review` until a plugin describes itself.
    pub(super) fn noun(&self, plural: bool) -> &str {
        match (&self.naming, plural) {
            (Some(naming), false) => &naming.noun,
            (Some(naming), true) => &naming.nouns,
            (None, false) => "review",
            (None, true) => "reviews",
        }
    }

    /// How the review whose id shows as `shown` reads in titles and
    /// messages: `PR #7` in the plugin's words.
    pub(super) fn review_label(&self, shown: &str) -> String {
        match &self.naming {
            Some(naming) => naming.label(shown),
            None => format!("review {shown}"),
        }
    }

    fn say_not_on_review(&mut self) {
        let said = match &self.naming {
            Some(n) => format!("not on a {}; dffy {} <id> opens one", n.noun, n.command),
            None => "not on a review".to_owned(),
        };
        self.message(said);
    }

    /// The run of `job` on the review `label` names, with `args`, unless a
    /// post is still running: a post can't be stopped halfway, and
    /// anything else would stop it.
    fn run(
        &mut self,
        plugin: String,
        (job, label): (Job, String),
        args: Vec<String>,
    ) -> Option<Action> {
        if self.job.as_ref().is_some_and(|r| r.job.posts()) {
            self.message("the review is still being posted; wait for it to finish");
            return None;
        }
        let id = self.next_run();
        self.job = Some(Running::new(id, job, label));
        let comments = match job {
            Job::Prepare => None,
            Job::Pull | Job::Post | Job::ApplyEvent(_) => {
                self.reviewing.as_ref().map(|p| p.session.comments.clone())
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

    /// The command panel's "Open review…", in the plugin's words: lists
    /// them to pick from, beside the run going on, which a look at the
    /// list shouldn't stop.
    pub(super) fn open_review_picker(&mut self) -> Option<Action> {
        let plugin = self.review_plugin()?;
        self.message(format!("listing {}…", self.noun(true)));
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

    /// Asks `plugin` whether the branch checked out has a review, for
    /// plain `dffy`; the viewer goes on meanwhile (see [`App::detected`]).
    pub(crate) fn detect_review(&mut self, plugin: String) -> Action {
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

    /// What `detect` said: a review for the branch is prepared, then its
    /// comments join plain `dffy`'s range, unless another review was opened
    /// meanwhile.
    fn detected(&mut self, said: Said) -> Option<Action> {
        match said {
            Said::Detected(Some(found)) if self.reviewing.is_none() && self.job.is_none() => {
                let plugin = self.review.clone()?;
                let label = self.review_label(found.shown());
                let action = self.run(
                    plugin,
                    (Job::Prepare, label),
                    vec!["prepare".to_owned(), found.id],
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
                    let noun = self.noun(false);
                    self.message(format!("looking for this branch's {noun} failed: {e}"));
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

    /// `p`: fetches the review's head and threads, and shows the new head
    /// in place.
    pub(super) fn update_review(&mut self) -> Option<Action> {
        let Some(mode) = &self.reviewing else {
            self.say_not_on_review();
            return None;
        };
        let (plugin, id, label) = (
            mode.plugin.clone(),
            mode.session.id.clone(),
            mode.label.clone(),
        );
        if self.job.as_ref().is_some_and(|r| r.job == Job::Pull) {
            self.message(format!("already updating {label}"));
            return None;
        }
        self.message(format!("updating {label}…"));
        self.run(plugin, (Job::Pull, label), vec!["pull".to_owned(), id])
    }

    /// `P`: the dialog that posts the review, and the dry run that says
    /// what posting would do, beside whatever else runs.
    pub(super) fn open_post(&mut self) -> Option<Action> {
        let Some(mode) = &self.reviewing else {
            self.say_not_on_review();
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
        let summary = TextBox::new(&mode.summary);
        let (plugin, review, comments) = (
            mode.plugin.clone(),
            mode.session.id.clone(),
            mode.session.comments.clone(),
        );
        let events =
            (self.naming.as_ref()).map_or_else(review::default_events, |n| n.events.clone());
        let id = self.next_run();
        self.posting = Some(Dialog {
            events,
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
            args: vec!["post".to_owned(), review, "--dry-run".to_owned()],
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
    /// on to the head, or back to what showed before.
    pub(super) fn toggle_since_review(&mut self) -> Option<Action> {
        let whole = format!("the whole {}", self.noun(false));
        let Some(mode) = &mut self.reviewing else {
            self.say_not_on_review();
            return None;
        };
        let Some(last) = mode.session.last_reviewed_head.clone() else {
            let note = format!("no review of yours posted from dffy on {} yet", mode.label);
            self.message(note);
            return None;
        };
        if last == mode.session.head && !mode.since_last {
            self.message("nothing new since your last review");
            return None;
        }
        mode.since_last = !mode.since_last;
        let note = if mode.since_last {
            format!(
                "since your last review: {}..{}",
                short(&last),
                short(&mode.session.head)
            )
        } else if mode.plain.is_some() && !mode.whole {
            "the working tree's changes".to_owned()
        } else {
            whole
        };
        self.message(note);
        self.retarget(None, None)
    }

    /// Shows the review's range, as it stands now, and its comments when
    /// `comments` names another file.
    fn retarget(
        &mut self,
        comments: Option<PathBuf>,
        moved: Option<(String, String)>,
    ) -> Option<Action> {
        Some(Action::Retarget {
            revisions: Some(self.review_range()?),
            comments,
            moved,
        })
    }

    /// "Show whole review": on the review plain `dffy` found, the review's
    /// base against the working tree, or plain `dffy`'s range again.
    pub(super) fn toggle_whole_review(&mut self) -> Option<Action> {
        let Some(mode) = &mut self.reviewing else {
            self.say_not_on_review();
            return None;
        };
        if mode.plain.is_none() {
            let note = format!("{} is shown whole already", mode.label);
            self.message(note);
            return None;
        }
        mode.whole = !mode.whole;
        mode.since_last = false;
        let note = if mode.whole {
            format!("{} + working tree", mode.label)
        } else {
            "the working tree's changes".to_owned()
        };
        self.message(note);
        self.retarget(None, None)
    }

    /// The command panel's words for [`App::toggle_whole_review`].
    pub(super) fn whole_review_title(&self) -> String {
        if self.reviewing.as_ref().is_some_and(|p| p.whole) {
            "Show working tree changes".to_owned()
        } else {
            format!("Show whole {}", self.noun(false))
        }
    }

    /// The review's range as it stands now, which the title and the list
    /// of files take.
    fn review_range(&mut self) -> Option<Revisions> {
        let mode = self.reviewing.as_ref()?;
        let revisions = mode.revisions();
        self.description = mode.description();
        if let Some((selected, _)) = &mut self.selected {
            selected.clone_from(&revisions);
        }
        Some(revisions)
    }

    /// The head of your last review, while "Since last review" shows from
    /// it.
    pub(crate) fn reviewed_from(&self) -> Option<&str> {
        let mode = self.reviewing.as_ref().filter(|p| p.since_last)?;
        mode.session.last_reviewed_head.as_deref()
    }

    /// The head of your last review isn't in the clone, so the range from
    /// it can't be listed: back to the whole review, whose range is
    /// returned.
    pub(crate) fn reviewed_head_missing(&mut self) -> Option<Revisions> {
        let mode = self.reviewing.as_mut()?;
        mode.since_last = false;
        let last = mode.session.last_reviewed_head.clone().unwrap_or_default();
        self.message(format!(
            "last reviewed commit {} isn't in this clone (force-pushed away?)",
            short(&last)
        ));
        self.review_range()
    }

    /// Whether "Since last review" is on, for the command panel.
    pub(super) fn since_review(&self) -> bool {
        self.reviewing.as_ref().is_some_and(|p| p.since_last)
    }

    /// The check, when it's due: none while the review's head is being
    /// fetched.
    pub(super) fn due_check(&mut self) -> Option<Action> {
        let busy = matches!(
            self.job.as_ref().map(|r| r.job),
            Some(Job::Pull | Job::Prepare)
        );
        let id = self.run_id + 1;
        let mode = self.reviewing.as_mut()?;
        if mode.check.is_some() || Instant::now() < mode.next_check {
            return None;
        }
        if busy {
            mode.next_check = Instant::now() + CHECK_EVERY;
            return None;
        }
        mode.check = Some(id);
        self.run_id = id;
        Some(Action::Plugin {
            id,
            plugin: mode.plugin.clone(),
            args: vec!["check".to_owned(), mode.session.id.clone()],
            comments: Some(mode.session.comments.clone()),
            summary: None,
        })
    }

    /// When the next check is due, while none runs.
    pub(super) fn check_deadline(&self) -> Option<Instant> {
        self.reviewing
            .as_ref()
            .filter(|p| p.check.is_none())
            .map(|p| p.next_check)
    }

    /// `x` on a thread in review mode, for the post dialog's count.
    pub(super) fn note_resolve(&mut self, root: &str, was: bool) {
        if let Some(mode) = &mut self.reviewing {
            match mode.resolves.get(root) {
                Some(&before) if before != was => {
                    mode.resolves.remove(root);
                }
                Some(_) => {}
                None => {
                    mode.resolves.insert(root.to_owned(), was);
                }
            }
        }
    }

    /// What a run said. Runs no longer wanted say nothing.
    pub(super) fn review_said(&mut self, id: u64, said: Said) -> Option<Action> {
        if self.reviewing.as_ref().is_some_and(|p| p.check == Some(id)) {
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
                held,
                held_message,
                url,
            } => {
                running.posted = Some((posted, failed, held));
                running.held_message = held_message;
                self.link = url.filter(|u| is_web(u));
            }
            Said::Done(result) => {
                let running = self.job.take()?;
                match (job, result) {
                    (Job::Post | Job::ApplyEvent(_), Ok(())) => {
                        if let Job::ApplyEvent(failed) = job {
                            self.applied(failed);
                        } else {
                            self.posted(running);
                        }
                        // The plugin's notice, read as the run ended, says
                        // more than the outcome, which the status bar keeps.
                        if let Some(notice) = self.comments.as_ref().and_then(Comments::notice) {
                            self.message(escape(notice.as_bytes()));
                        }
                    }
                    (_, Ok(())) => {}
                    (job, Err(e)) => {
                        let e = escape(e.as_bytes());
                        let label = &running.label;
                        self.message(match job {
                            Job::Prepare => format!("{label}: {e}"),
                            Job::Pull => format!("updating {label} failed: {e}"),
                            Job::Post => format!("posting failed: {e}"),
                            Job::ApplyEvent(_) => format!("sending the review failed: {e}"),
                        });
                    }
                }
            }
            Said::Listed(_) | Said::Head { .. } | Said::Detected(_) | Said::Planned { .. } => {}
        }
        None
    }

    /// A post that finished: what it did goes in the status bar, unless the
    /// host refused drafts and so the submitting event picked was held
    /// back, which asks first. Drafts the dialog showed couldn't be posted
    /// don't hold the event back.
    fn posted(&mut self, running: Running) {
        let Some(mode) = &mut self.reviewing else {
            return;
        };
        let Some(event) = mode.event.clone() else {
            return;
        };
        let (posted, failed, held) = running.posted.unwrap_or_default();
        // As the plugin records it: an event that leaves the review open,
        // with nothing in it, reviewed nothing.
        if posted > 0 || (event.submits && !held) {
            mode.session.last_reviewed_head = Some(mode.session.head.clone());
        }
        mode.resolves.clear();
        let message = held.then(|| running.held_message.unwrap_or_else(|| HELD.to_owned()));
        let text = outcome(&event, (posted, failed), message.as_deref());
        if event.submits && held && failed > 0 {
            mode.outcome = Some(text);
            self.held = Some(Held {
                failed,
                unposted: running.unposted,
                event,
                message,
            });
            self.message = None;
            return;
        }
        mode.summary.clear();
        mode.outcome = Some(text.clone());
        // The link goes in the note only: it would crowd the status bar
        // out of room for the review's part.
        match self.link.as_deref().filter(|_| failed == 0) {
            Some(url) => self.message(format!("{text} · {url}")),
            None => self.message = None,
        }
    }

    /// `post --apply-event` finished: the event went without the drafts
    /// left out.
    fn applied(&mut self, failed: usize) {
        let Some(mode) = &mut self.reviewing else {
            return;
        };
        mode.session.last_reviewed_head = Some(mode.session.head.clone());
        mode.summary.clear();
        if let Some(event) = &mode.event {
            mode.outcome = Some(outcome(event, (0, failed), None));
        }
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
        let held = self.held.as_ref()?;
        let (failed, event) = (held.failed, held.event.id.clone());
        match key {
            Key::Char('s') => {
                self.held = None;
                let current = self.reviewing.as_ref()?;
                let (plugin, id, label, summary) = (
                    current.plugin.clone(),
                    current.session.id.clone(),
                    current.label.clone(),
                    current.summary.clone(),
                );
                self.message("sending the review without them…");
                let mut action = self.run(
                    plugin,
                    (Job::ApplyEvent(failed), label),
                    vec![
                        "post".to_owned(),
                        id,
                        "--apply-event".to_owned(),
                        "--event".to_owned(),
                        event,
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
            Said::Listed(reviews) => {
                self.message = None;
                self.open_picker_of(reviews);
            }
            Said::Done(result) => {
                self.listing = None;
                if let Err(e) = result {
                    let e = escape(e.as_bytes());
                    self.message(format!("listing {} failed: {e}", self.noun(true)));
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    /// A session line: a review opened from the picker or found for the
    /// branch (`local`), or the review updated.
    fn prepared(&mut self, job: Job, local: bool, session: Session) -> Option<Action> {
        let plugin = self.review.clone().unwrap_or_default();
        let label = self.review_label(session.shown());
        match job {
            Job::Prepare => {
                let comments = session.comments.clone();
                let title = session.title.clone().unwrap_or_default();
                let mut mode = Mode::new(plugin, session, label.clone());
                if local {
                    let revisions = self.selected.as_ref().map(|(r, _)| r.clone());
                    mode.plain = Some((revisions.unwrap_or_default(), self.description.clone()));
                }
                // The retarget takes the session's comments file, read as
                // a session's from here on.
                self.reviewing = Some(mode);
                let on = if local { " · this branch's" } else { "" };
                self.message(format!("{label}{on} · {}", escape(title.as_bytes())));
                if local {
                    // The range stays plain dffy's: only the comments change.
                    return Some(Action::Retarget {
                        revisions: None,
                        comments: Some(comments),
                        moved: None,
                    });
                }
                self.retarget(Some(comments), None)
            }
            Job::Pull => {
                let mode = self.reviewing.as_mut()?;
                mode.moved = None;
                let old = std::mem::replace(&mut mode.session, session);
                mode.label.clone_from(&label);
                let s = &mode.session;
                let comments = (s.comments != old.comments).then(|| s.comments.clone());
                if (&s.base, &s.head) == (&old.base, &old.head) && comments.is_none() {
                    self.message(format!("{label} is up to date"));
                    return None;
                }
                let moved = (s.head != old.head).then(|| (old.head.clone(), s.head.clone()));
                self.message(format!("{label} updated"));
                self.retarget(comments, moved)
            }
            Job::Post | Job::ApplyEvent(_) => None,
        }
    }

    /// After an update moved the head from `old` to `new`, how many
    /// commits that is, when git could tell.
    pub(crate) fn review_updated(&mut self, count: Option<usize>) {
        let (Some(mode), Some(count)) = (&self.reviewing, count) else {
            return;
        };
        let s = if count == 1 { "" } else { "s" };
        let note = format!("{} updated · {count} new commit{s}", mode.label);
        self.message(note);
    }

    fn checked(&mut self, said: Said) {
        let Some(mode) = &mut self.reviewing else {
            return;
        };
        match said {
            Said::Head { head, moved } => {
                mode.moved = (moved && head != mode.session.head).then_some(head);
                self.dirty = true;
            }
            Said::Done(result) => {
                mode.check = None;
                mode.next_check = Instant::now() + CHECK_EVERY;
                if let Err(e) = result {
                    let e = escape(e.as_bytes());
                    let note = format!("checking {} failed: {e}", mode.label);
                    self.message(note);
                }
            }
            _ => {}
        }
    }

    /// The review's part of the status bar: `PR #7`, what's running, and
    /// whether the head moved.
    pub(super) fn review_part(&self) -> Option<String> {
        let mode = self.reviewing.as_ref()?;
        let mut text = mode.label.clone();
        if let Some(running) = &self.job {
            let doing = match running.job {
                Job::Prepare => "fetching threads",
                Job::Pull => "updating",
                Job::Post => "posting",
                Job::ApplyEvent(_) => "sending",
            };
            let _ = write!(text, " · {doing}");
            if let Some(progress) = &running.progress {
                let _ = write!(text, " {progress}");
            }
        } else if mode.moved.is_some() {
            text.push_str(" updated · new commits · p to update");
        }
        if let Some(outcome) = &mode.outcome {
            let _ = write!(text, " · {outcome}");
        }
        if self.link.is_some() {
            text.push_str(" · o opens it");
        }
        Some(text)
    }

    /// Input that review mode's overlays take, or `o` while a posted review's
    /// note shows; `Continue` when it's for the rest of the viewer.
    pub(super) fn review_input(&mut self, input: &Input) -> ControlFlow<Option<Action>> {
        if self.posting.is_some() {
            return ControlFlow::Break(self.post_input(input));
        }
        if self.held.is_some() && !self.keys_elsewhere() {
            return ControlFlow::Break(self.held_input(input));
        }
        if self.reviews.is_some() {
            return ControlFlow::Break(self.review_picker_input(input));
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
                if let (Some(mode), Some(dialog)) = (&mut self.reviewing, self.posting.take()) {
                    dialog.summary.text().clone_into(&mut mode.summary);
                }
            }
            Key::Tab | Key::Char('s') => dialog.writing = true,
            Key::Up | Key::Char('k') => dialog.event = dialog.event.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                dialog.event = (dialog.event + 1).min(dialog.events.len().saturating_sub(1));
            }
            Key::Char(c) if dialog.events.iter().any(|e| e.key == c) => {
                dialog.event = dialog.events.iter().position(|e| e.key == c).unwrap_or(0);
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
                if let (Some(mode), Some(dialog)) = (&mut self.reviewing, self.posting.take()) {
                    dialog.summary.text().clone_into(&mut mode.summary);
                }
                self.go_to_unposted(&skipped);
            }
            Key::Enter => return self.send_review(),
            _ => {}
        }
        None
    }

    /// Posts the review as the dialog has it, once the local drafts in view
    /// are in the review's comments file, where `post` reads them. When
    /// they can't be moved, nothing is posted and the dialog stays.
    fn send_review(&mut self) -> Option<Action> {
        let moved = (self.comments.as_mut())
            .filter(|c| c.local_drafts() > 0)
            .map(Comments::move_local);
        match moved {
            Some(Ok(changed)) => self.placed_again(&changed),
            Some(Err(e)) => {
                let e = escape(e.as_bytes());
                let noun = self.noun(false);
                self.message(format!(
                    "nothing posted: local drafts not moved to the {noun}: {e}"
                ));
                return None;
            }
            None => {}
        }
        let mut dialog = self.posting.take()?;
        let mode = self.reviewing.as_mut()?;
        let summary = dialog.summary.text().trim_end().to_owned();
        mode.summary.clone_from(&summary);
        let (plugin, id, label) = (
            mode.plugin.clone(),
            mode.session.id.clone(),
            mode.label.clone(),
        );
        let event = dialog.events.swap_remove(dialog.event);
        let event_id = event.id.clone();
        mode.event = Some(event);
        mode.outcome = None;
        self.message(format!("posting the review on {label}…"));
        let mut action = self.run(
            plugin,
            (Job::Post, label),
            vec!["post".to_owned(), id, "--event".to_owned(), event_id],
        );
        if let Some(Action::Plugin { summary: s, .. }) = &mut action {
            *s = (!summary.is_empty()).then_some(summary);
        }
        action
    }

    /// What `P` would send: new comments, replies and resolve changes, and
    /// how many of the comments and replies are local drafts.
    pub(super) fn unsent(&self) -> Unsent {
        let (comments, replies) = self.comments.as_ref().map_or((0, 0), Comments::unsent);
        Unsent {
            comments,
            replies,
            resolves: self.reviewing.as_ref().map_or(0, |p| p.resolves.len()),
            local: self.comments.as_ref().map_or(0, Comments::local_drafts),
        }
    }
}

/// What `P` would send (see [`App::unsent`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Unsent {
    pub comments: usize,
    pub replies: usize,
    pub resolves: usize,
    /// Comments and replies among them from the local review's file, which
    /// the plugin's dry run doesn't see: they move to the review's file only
    /// as the review is sent.
    pub local: usize,
}

impl Preflight {
    /// What goes: the plugin's count of drafts once its dry run is in, in
    /// which replies are comments, plus the local drafts it doesn't see;
    /// else dffy's own count.
    fn what(&self, unsent: Unsent) -> String {
        let Unsent {
            comments,
            replies,
            resolves,
            local,
        } = unsent;
        let mut parts = Vec::new();
        let of_which = |parts: &mut Vec<String>| {
            if local > 0
                && let Some(last) = parts.last_mut()
            {
                let _ = write!(last, " ({local} local)");
            }
        };
        if let Some((line, file, skip)) = self.planned {
            if line + file + skip + local > 0 {
                parts.push(plural(line + file + skip + local, "comment"));
            }
            of_which(&mut parts);
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
            of_which(&mut parts);
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
    /// Draws the dialog for `mode`, which would send `unsent` (see
    /// [`App::unsent`]).
    pub(super) fn draw(&mut self, grid: &mut Grid, theme: &Theme, mode: &Mode, unsent: Unsent) {
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
        lines.push((dialog.preflight.what(unsent), Style::PLAIN));
        lines.extend(
            dialog
                .preflight
                .note()
                .map(|(text, warn)| (text, if warn { theme.style(Role::Warn) } else { dim })),
        );
        if mode.moved.is_some() {
            lines.push((
                format!(
                    "! {} has new commits; this review goes on the head shown",
                    mode.label
                ),
                theme.style(Role::Warn),
            ));
        }
        lines.push((String::new(), Style::PLAIN));
        let first_event = lines.len();
        for (i, event) in dialog.events.iter().enumerate() {
            let dot = if i == dialog.event { "(•)" } else { "( )" };
            lines.push((
                format!("{dot} {}  {}", event.key, event.label),
                Style::PLAIN,
            ));
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
        let title = format!(" post review · {} ", mode.label);
        help::frame(grid, (x0, y0, width, height), &title, theme);
        let (x, right) = (x0 + 2, x0 + width - 2);
        for (i, (text, style)) in lines.iter().enumerate() {
            let y = y0 + 1 + i;
            if y + 2 >= y0 + height {
                break;
            }
            grid.put_str(x, y, right, text, *style);
            let event = (i.checked_sub(first_event)).filter(|&e| e < dialog.events.len());
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
    pub(super) fn draw(&self, grid: &mut Grid, theme: &Theme, mode: &Mode) {
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
        let label = &self.event.label;
        lines.push((String::new(), Style::PLAIN));
        lines.push((
            format!("so “{label}” wasn't applied"),
            theme.style(Role::Warn),
        ));
        if let Some(message) = &self.message {
            lines.push((escape(message.as_bytes()), dim));
        }
        lines.extend([
            (String::new(), Style::PLAIN),
            (format!("s  {label} without them"), Style::PLAIN),
            (
                "f  fix them: go to the first, the review stays as it is".to_owned(),
                Style::PLAIN,
            ),
            ("k  leave the review as it is".to_owned(), Style::PLAIN),
        ]);
        let height = (lines.len() + 4).min(gh);
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        let title = format!(" review not sent · {} ", mode.label);
        help::frame(grid, (x0, y0, width, height), &title, theme);
        let (x, right) = (x0 + 2, x0 + width - 2);
        for (i, (text, style)) in lines.iter().enumerate() {
            let y = y0 + 1 + i;
            if y + 2 >= y0 + height {
                break;
            }
            grid.put_str(x, y, right, text, *style);
        }
        grid.put_str(x, y0 + height - 2, right, "s, f or k · esc leaves it", dim);
    }
}

impl App {
    /// Input while the review picker is open: `j`/`k` or the arrows move,
    /// `enter` opens, `esc` closes.
    pub(super) fn review_picker_input(&mut self, input: &Input) -> Option<Action> {
        let picker = self.reviews.as_mut()?;
        let last = picker.reviews.len().saturating_sub(1);
        match *input {
            Input::Key(Key::Char('c'), mods) if mods.contains(Mods::CTRL) => {
                return Some(Action::Quit);
            }
            Input::Key(Key::Esc | Key::Char('q'), _) => self.reviews = None,
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
                let listed = picker.reviews.get(picker.cursor)?;
                let (id, shown) = (listed.id.clone(), listed.shown().to_owned());
                self.reviews = None;
                let label = self.review_label(&shown);
                return self.open_review(id, label);
            }
            _ => {}
        }
        None
    }

    fn open_review(&mut self, id: String, label: String) -> Option<Action> {
        if self.reviewing.as_ref().is_some_and(|p| p.session.id == id) {
            self.message(format!("{label} is open"));
            return None;
        }
        let plugin = self.review_plugin()?;
        self.message(format!("fetching {label}…"));
        self.run(
            plugin,
            (Job::Prepare, label),
            vec!["prepare".to_owned(), id],
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
        // Group titles, then each review under its group's.
        let mut lines: Vec<Option<usize>> = Vec::new();
        let mut titles: Vec<&str> = Vec::new();
        let mut last = None;
        for (i, &group) in picker.group.iter().enumerate() {
            if last != Some(group) {
                if last.is_some() {
                    lines.push(None);
                    titles.push("");
                }
                lines.push(None);
                titles.push(&picker.groups[group]);
                last = Some(group);
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
        help::frame(grid, (x0, y0, width, height), &picker.title, theme);
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
            let listed = &picker.reviews[i];
            let selected = i == picker.cursor;
            let mark = |s: Style| if selected { theme.selected(s, true) } else { s };
            if selected {
                grid.fill(x - 1, y, right + 1, mark(Style::PLAIN));
            }
            let after = grid.put_str(
                x,
                y,
                right,
                &format!("{:<w$}", picker.tags[i], w = picker.tag_width),
                mark(theme.style(Role::Hint)),
            );
            let author = format!("  {}", escape(listed.author.as_bytes()));
            let title_end = right.saturating_sub(author.width());
            grid.put_str(
                after + 1,
                y,
                title_end,
                &escape(listed.title.as_bytes()),
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
