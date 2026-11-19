//! Review mode: the viewer on a review that a review plugin prepared,
//! showing `<base>...<head>` with the review's comments file. The plugin
//! runs in the background (see [`crate::plugin::review`]): what it says
//! comes back as [`Event::Review`] for the run's id, and runs that are no
//! longer wanted are ignored. Every word naming the host's kind of review is the plugin's.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dffy_core::git::Revisions;
use dffy_core::view::Attributes;
use unicode_width::UnicodeWidthStr;

use super::{Action, App, Comments, escape, is_web};
use crate::plugin::review::{self, Listed, Plan, Reason, ReviewEvent, Said, Session, State};
use crate::term::{self, Grid, Input, Key, Mods};
use crate::theme::{Role, Style, Theme};
use crate::tui::help;
use crate::tui::textbox::TextBox;

/// How often the plugin is asked whether the review's head moved.
pub(super) const CHECK_EVERY: Duration = Duration::from_secs(60);

/// No input for this long stops the checks until the next key: a viewer
/// left open overnight shouldn't ask the host all night.
pub(super) const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long after the drafts change their plan is asked for again, so a
/// burst of edits or a plugin's page of threads asks once.
const PLAN_AFTER: Duration = Duration::from_millis(400);

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
    /// The event the last post sent, which the dialog starts on when the
    /// plugin's events have none that submits.
    pub event: Option<ReviewEvent>,
    /// How the last post ended, which the status bar keeps saying.
    pub outcome: Option<String>,
    /// The head on the host, when the last check found it moved.
    pub moved: Option<String>,
    /// The last check found the review changed on the host since its
    /// comments were imported: new comments, most likely.
    pub comments_moved: bool,
    /// The check running, by its run's id.
    pub check: Option<u64>,
    pub next_check: Instant,
    /// Why the last check failed, said once until it changes.
    pub check_failed: Option<String>,
    /// When the session started, seconds since 1970: local drafts written
    /// since are this review's, and the dialog sends them unasked.
    pub started: u64,
    pub plans: Plans,
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
            comments_moved: false,
            check: None,
            next_check: Instant::now() + CHECK_EVERY,
            check_failed: None,
            started: super::now(),
            plans: Plans::default(),
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

    /// Closed or merged on the host, when the plugin says so.
    fn ended(&self) -> Option<State> {
        self.session.state.filter(|s| *s != State::Open)
    }
}

/// What `post --dry-run` last said posting would do with each draft. It
/// runs in the background after the drafts change, never while a post
/// does, and its answer goes into the drafts' headings and the dialog.
#[derive(Default)]
pub(super) struct Plans {
    /// The dry run going on, by its run's id, and what it said so far.
    running: Option<(u64, Answer)>,
    /// The last whole answer.
    known: Option<Answer>,
    /// Why the last run failed, until one doesn't.
    failed: Option<String>,
    /// When the next is to run: the drafts changed since the last.
    pub due: Option<Instant>,
}

/// Where the dialog's counts stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Checked {
    /// A plan answered since the drafts last changed.
    Yes,
    Checking,
    /// The last plan failed, saying this.
    Failed(String),
}

#[derive(Default, Clone)]
struct Answer {
    drafts: HashMap<String, Plan>,
    pending: usize,
    resolves: usize,
}

/// A plugin run that isn't a `check`, a `list` or a dry run; one at a
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Job {
    Prepare,
    Pull,
    Post,
}

impl Job {
    fn doing(self) -> &'static str {
        match self {
            Self::Prepare => "fetching threads",
            Self::Pull => "updating",
            Self::Post => "posting",
        }
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
    /// A post's drafts not posted, by id, with why.
    pub unposted: Vec<(String, String)>,
    /// A post's last line: comments posted, and not.
    pub posted: Option<(usize, usize)>,
    /// For a `prepare` or `pull`: the posted threads' roots by ref as its
    /// first word came, to tell the new ones at its end; none when there's
    /// nothing to tell them from, as on a first open.
    pub before: Option<HashMap<String, String>>,
    /// `before` was taken, or found to have nothing.
    pub looked: bool,
    /// A `pull` whose session line named the head already shown.
    pub same: bool,
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
            before: None,
            looked: false,
            same: false,
        }
    }
}

/// The dialog `P` opens, as GitHub's "Finish your review": a summary, the
/// plugin's events, and one line when something won't go.
pub(super) struct Dialog {
    /// The plugin's events, and the one picked.
    events: Vec<ReviewEvent>,
    event: usize,
    summary: TextBox,
    /// Keys go to the summary.
    writing: bool,
    /// The local drafts in view go with the review (`l`).
    local: bool,
}

/// What the dialog says goes, from the drafts and the last plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Sending {
    /// Drafts that go, replies included.
    pub comments: usize,
    /// Of them, on the whole file.
    pub file: usize,
    /// Of them, waiting for their lines to be on the host.
    pub waits: usize,
    /// Of them, refused by the host before.
    pub refused: usize,
    /// Local drafts in view, whether or not they go.
    pub local: usize,
    /// Threads resolved or reopened and not sent, once a plan said.
    pub resolves: Option<usize>,
    /// Comments in your pending review on the host, which a submitting
    /// event sends.
    pub pending: usize,
}

/// How a post of `event` ended, in the words the status bar keeps:
/// `review sent`, `review sent · 2 comments refused, kept as drafts`, the
/// event's own words for one that leaves the review open, or `nothing
/// posted`.
fn outcome(event: &ReviewEvent, posted: usize, (refused, waits): (usize, usize)) -> String {
    let mut text = if event.submits {
        "review sent".to_owned()
    } else if posted + refused + waits == 0 {
        "nothing posted".to_owned()
    } else {
        event.done.clone()
    };
    if refused > 0 {
        let _ = write!(
            text,
            " · {} refused, kept as drafts",
            plural(refused, "comment")
        );
    }
    if waits > 0 {
        let wait = if waits == 1 { "waits" } else { "wait" };
        let _ = write!(text, " · {} {wait} for a push", plural(waits, "draft"));
    }
    text
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
    fn new(reviews: Vec<Listed>, nouns: &str, prefix: &str) -> Self {
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
        let mut keyed: Vec<(Reason, usize, Listed)> = (reviews.into_iter())
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

pub(super) fn plural(n: usize, one: &str) -> String {
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
        self.plan_soon();
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

    /// Whether a post is running: it can't be stopped halfway.
    pub(crate) fn posting_now(&self) -> bool {
        self.job.as_ref().is_some_and(|r| r.job == Job::Post)
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
        if self.posting_now() {
            self.message("the review is still being posted; wait for it to finish");
            return None;
        }
        let id = self.next_run();
        self.job = Some(Running::new(id, job, label));
        let comments = match job {
            Job::Prepare => None,
            Job::Pull | Job::Post => self.reviewing.as_ref().map(|p| p.session.comments.clone()),
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

    /// `P`: the dialog that posts the review, with the summary kept from
    /// before and the first event that submits, else the one last sent. It
    /// asks for a plan beside whatever else runs, unless the last still
    /// holds.
    pub(super) fn open_post(&mut self) -> Option<Action> {
        let Some(mode) = &self.reviewing else {
            self.say_not_on_review();
            return None;
        };
        if let Some(running) = self.job.as_ref().filter(|r| r.job == Job::Post) {
            let at = running
                .progress
                .as_deref()
                .map(|p| format!(" {p}"))
                .unwrap_or_default();
            self.message(format!("posting{at}…"));
            return None;
        }
        let events =
            (self.naming.as_ref()).map_or_else(review::default_events, |n| n.events.clone());
        let last = mode.event.as_ref().map(|e| e.id.as_str());
        let event = events
            .iter()
            .position(|e| e.submits)
            .or_else(|| events.iter().position(|e| Some(e.id.as_str()) == last))
            .unwrap_or(0);
        let started = mode.started;
        let (summary, local) = self.comments.as_ref().map_or((String::new(), true), |c| {
            (
                c.summary().unwrap_or_default().to_owned(),
                c.local_drafts_before(started) == 0,
            )
        });
        self.posting = Some(Dialog {
            events,
            event,
            summary: TextBox::new(&summary),
            writing: false,
            local,
        });
        let plans = &self.reviewing.as_ref()?.plans;
        if plans.running.is_none() && (plans.known.is_none() || plans.due.is_some()) {
            return self.start_plan();
        }
        None
    }

    /// The drafts' plan, due soon: they changed.
    pub(super) fn plan_soon(&mut self) {
        if let Some(mode) = &mut self.reviewing {
            mode.plans.due = Some(Instant::now() + PLAN_AFTER);
        }
    }

    /// When the plan due should run, while it can: none runs, no post
    /// does, and there are drafts to plan.
    pub(super) fn plan_deadline(&self) -> Option<Instant> {
        let plans = &self.reviewing.as_ref()?.plans;
        let due = plans.due?;
        let drafts = || self.comments.as_ref().is_some_and(|c| c.counts().1 > 0);
        (plans.running.is_none() && !self.posting_now() && drafts()).then_some(due)
    }

    /// The plan, once it's due.
    pub(super) fn due_plan(&mut self) -> Option<Action> {
        let due = self.plan_deadline()?;
        if Instant::now() < due {
            return None;
        }
        self.start_plan()
    }

    /// `post --dry-run`, with the local review's drafts, which the plugin
    /// plans after the review's own.
    fn start_plan(&mut self) -> Option<Action> {
        let id = self.run_id + 1;
        let local = self
            .comments
            .as_ref()
            .and_then(|c| c.sources().get(1).cloned());
        let mode = self.reviewing.as_mut()?;
        self.run_id = id;
        mode.plans.due = None;
        mode.plans.running = Some((id, Answer::default()));
        let mut args = vec![
            "post".to_owned(),
            mode.session.id.clone(),
            "--dry-run".to_owned(),
        ];
        if let Some(local) = local {
            args.extend(["--drafts".to_owned(), local.display().to_string()]);
        }
        Some(Action::Plugin {
            id,
            plugin: mode.plugin.clone(),
            args,
            comments: Some(mode.session.comments.clone()),
            summary: None,
        })
    }

    /// What the dry run says. A failed one says nothing, which the dialog
    /// shows as not knowing; the last answer stays.
    fn planned(&mut self, said: Said) {
        let short = self.naming.as_ref().map_or("review", |n| n.short.as_str());
        let waits = format!("not in the {short} yet");
        let Some(mode) = &mut self.reviewing else {
            return;
        };
        let Some((_, answer)) = &mut mode.plans.running else {
            return;
        };
        match said {
            Said::Draft(draft) => {
                answer.drafts.insert(draft.id, draft.plan);
            }
            Said::Planned {
                pending, resolves, ..
            } => {
                answer.pending = pending;
                answer.resolves = resolves;
            }
            Said::Done(result) => {
                let answer = mode.plans.running.take().map(|(_, a)| a);
                match result {
                    Ok(()) => {
                        mode.plans.known = answer;
                        mode.plans.failed = None;
                    }
                    Err(e) => mode.plans.failed = Some(e),
                }
                let words =
                    mode.plans
                        .known
                        .iter()
                        .flat_map(|a| &a.drafts)
                        .filter_map(|(id, plan)| {
                            let word = match plan {
                                Plan::Line => return None,
                                Plan::File => (Role::Dim, "posts as file comment".to_owned()),
                                Plan::Skip(_) => (Role::Warn, waits.clone()),
                            };
                            Some((id.clone(), word))
                        });
                let words = words.collect();
                let changed = self
                    .comments
                    .as_mut()
                    .map_or_else(Vec::new, |c| c.set_plans(words));
                if !changed.is_empty() {
                    self.placed_again(&changed);
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    /// What the dialog says goes: the drafts in view, the local ones only
    /// with `local`, counted by what the last plan said of each.
    pub(super) fn sending(&self, local: bool) -> Sending {
        let Some(comments) = &self.comments else {
            return Sending::default();
        };
        let known = self.reviewing.as_ref().and_then(|m| m.plans.known.as_ref());
        let mut out = Sending {
            pending: known.map_or_else(|| comments.host_pending(), |k| k.pending),
            resolves: known.map(|k| k.resolves),
            ..Sending::default()
        };
        for (id, from_local, refused) in comments.drafts() {
            out.local += usize::from(from_local);
            if from_local && !local {
                continue;
            }
            out.comments += 1;
            out.refused += usize::from(refused);
            match known.and_then(|k| k.drafts.get(id)) {
                Some(Plan::File) => out.file += 1,
                Some(Plan::Skip(_)) => out.waits += 1,
                Some(Plan::Line) | None => {}
            }
        }
        out
    }

    /// The drafts that won't go as they are, with why: those the plan
    /// skips, and those the host refused before.
    fn wont_go(&self, local: bool) -> Vec<(String, String)> {
        let (Some(comments), Some(mode)) = (&self.comments, &self.reviewing) else {
            return Vec::new();
        };
        let known = mode.plans.known.as_ref();
        comments
            .drafts()
            .into_iter()
            .filter(|&(_, from_local, _)| local || !from_local)
            .filter_map(|(id, _, refused)| {
                let why = match known.and_then(|k| k.drafts.get(id)) {
                    Some(Plan::Skip(why)) => why.clone(),
                    _ if refused => "refused before".to_owned(),
                    _ => return None,
                };
                Some((id.to_owned(), why))
            })
            .collect()
    }

    /// Focuses the thread of the first of `drafts` still in the file, and
    /// says why it won't go; `m`/`M` go on from there.
    fn go_to_unposted(&mut self, drafts: &[(String, String)]) {
        let Some(comments) = &self.comments else {
            return;
        };
        let Some(((_, why), root)) = drafts
            .iter()
            .find_map(|d| Some((d, comments.thread_root(&d.0)?)))
        else {
            self.message("the drafts left out aren't in the file any more");
            return;
        };
        let mut note = format!("not posted: {}", escape(why.as_bytes()));
        if drafts.len() > 1 {
            let _ = write!(
                note,
                " · {} more · m/M go between threads",
                drafts.len() - 1
            );
        }
        self.follow_thread(&root);
        self.leave_panel();
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
        let title = mode.session.title.as_deref().unwrap_or(&mode.label);
        title.clone_into(&mut self.doc.page_title);
        if let Some((selected, _)) = &mut self.selected {
            selected.clone_from(&revisions);
        }
        Some(revisions)
    }

    /// The head of your last review and the head, while "Since last review"
    /// shows from it.
    pub(crate) fn reviewed_from(&self) -> Option<(&str, &str)> {
        let mode = self.reviewing.as_ref().filter(|p| p.since_last)?;
        Some((
            mode.session.last_reviewed_head.as_deref()?,
            &mode.session.head,
        ))
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

    /// The review was rebased since your last review: the range from it
    /// takes in what the base gained too, which the note says.
    pub(crate) fn reviewed_head_rebased(&mut self) {
        let Some((last, head)) = self.reviewed_from() else {
            return;
        };
        let note = format!(
            "rebased since your last review; showing {}..{}, the base's changes since included",
            short(last),
            short(head)
        );
        self.message(note);
    }

    pub(super) fn since_review(&self) -> bool {
        self.reviewing.as_ref().is_some_and(|p| p.since_last)
    }

    /// The check, when it's due: none while the review's head is being
    /// fetched, nor while no key came for a while.
    pub(super) fn due_check(&mut self) -> Option<Action> {
        let busy = matches!(
            self.job.as_ref().map(|r| r.job),
            Some(Job::Pull | Job::Prepare)
        );
        let idle = self.last_input.elapsed() >= IDLE_AFTER;
        let id = self.run_id + 1;
        let mode = self.reviewing.as_mut()?;
        if mode.check.is_some() || Instant::now() < mode.next_check {
            return None;
        }
        // Past the idle mark the next check is beyond it, which wakes
        // nothing; a key brings it forward (see `App::resume_checks`).
        if busy || idle {
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

    /// A key after a while without one: the check is due now.
    pub(super) fn resume_checks(&mut self) {
        if let Some(mode) = &mut self.reviewing {
            mode.next_check = mode.next_check.min(Instant::now());
        }
    }

    /// When the next check is due, while none runs and keys come.
    pub(super) fn check_deadline(&self) -> Option<Instant> {
        let idle_at = self.last_input + IDLE_AFTER;
        self.reviewing
            .as_ref()
            .filter(|p| p.check.is_none() && p.next_check < idle_at)
            .map(|p| p.next_check)
    }

    /// Runs no longer wanted are ignored.
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
        if (self.reviewing.as_ref())
            .and_then(|p| p.plans.running.as_ref())
            .is_some_and(|(run, _)| *run == id)
        {
            self.planned(said);
            return None;
        }
        let running = self.job.as_mut().filter(|r| r.id == id)?;
        let (job, local) = (running.job, running.local);
        self.dirty = true;
        match said {
            Said::Progress(progress) => running.progress = Some(progress),
            Said::Session(session) => return self.prepared(job, local, session),
            Said::Draft(draft) => {
                if let Plan::Skip(why) = draft.plan {
                    running.unposted.push((draft.id, why));
                }
            }
            Said::Posted {
                posted,
                failed,
                url,
                ..
            } => {
                running.posted = Some((posted, failed));
                self.link = url.filter(|u| is_web(u));
            }
            Said::Head { head, moved, .. } => {
                if let Some(mode) = &mut self.reviewing {
                    mode.moved = (moved && head != mode.session.head).then_some(head);
                }
            }
            Said::Done(result) => {
                let running = self.job.take()?;
                self.plan_soon();
                match (job, result) {
                    (Job::Post, Ok(())) => {
                        self.posted(&running);
                        // The plugin's notice, read as the run ended, says
                        // more than the outcome, which the status bar keeps.
                        if let Some(notice) = self.comments.as_ref().and_then(Comments::notice) {
                            self.message(escape(notice.as_bytes()));
                        }
                        self.unseen = self.reviewing.as_ref().and_then(|mode| {
                            let mut outcome = mode.outcome.as_ref()?.chars();
                            let first = outcome.next()?.to_uppercase();
                            Some(term::Status {
                                state: term::State::Done,
                                message: Some(format!(
                                    "{}{} · {}",
                                    first,
                                    outcome.as_str(),
                                    mode.label
                                )),
                                ..term::Status::default()
                            })
                        });
                    }
                    (Job::Prepare | Job::Pull, Ok(())) => self.imported(&running),
                    (job, Err(e)) => {
                        let e = escape(e.as_bytes());
                        let label = &running.label;
                        self.failed(match job {
                            Job::Prepare => format!("{label}: {e}"),
                            Job::Pull => format!("updating {label} failed: {e}"),
                            Job::Post => format!("posting failed: {e}"),
                        });
                    }
                }
            }
            Said::Listed(_) | Said::Detected(_) | Said::Planned { .. } => {}
        }
        None
    }

    /// Before a `prepare`'s or `pull`'s word is read: the threads the
    /// comments file has, to tell the ones it brings. Nothing to tell them
    /// from on the first word of a `prepare` of a review whose file isn't
    /// shown yet, as when it was just picked or opened for the first time.
    pub(super) fn before_import(&mut self, id: u64) {
        let shown = self
            .comments
            .as_ref()
            .filter(|c| {
                self.reviewing
                    .as_ref()
                    .is_some_and(|p| c.sources().first() == Some(&p.session.comments))
            })
            .map(Comments::posted_roots);
        let Some(running) = self
            .job
            .as_mut()
            .filter(|r| r.id == id && r.job != Job::Post && !r.looked)
        else {
            return;
        };
        running.looked = true;
        running.before = shown.filter(|roots| !roots.is_empty());
    }

    /// A `prepare` or `pull` finished: the threads it brought, which `m`
    /// goes to first, or that the review is as it was.
    fn imported(&mut self, running: &Running) {
        if let Some(mode) = &mut self.reviewing {
            mode.comments_moved = false;
        }
        let new: HashSet<String> = match (&running.before, &self.comments) {
            (Some(before), Some(comments)) => comments
                .posted_roots()
                .into_iter()
                .filter(|(reference, _)| !before.contains_key(reference))
                .map(|(_, id)| id)
                .collect(),
            _ => HashSet::new(),
        };
        if let Some(comments) = self.comments.as_ref().filter(|_| !new.is_empty()) {
            self.new_threads = comments.in_order(&new);
            let next = self
                .key_for(false, crate::keys::Action::NextThread)
                .unwrap_or_default();
            let note = format!(
                "{} · {next} goes to them",
                plural(self.new_threads.len(), "new comment")
            );
            self.message(note);
        } else if running.same {
            self.message(format!("{} is up to date", running.label));
        }
    }

    /// A post that finished: what it did goes in the status bar. Drafts it
    /// left out stay drafts; one the host refused says why in its heading.
    fn posted(&mut self, running: &Running) {
        let refused = self.comments.as_ref().map_or(0, |c| {
            running
                .unposted
                .iter()
                .filter(|(id, _)| c.refused(id))
                .count()
        });
        let Some(mode) = &mut self.reviewing else {
            return;
        };
        let Some(event) = mode.event.clone() else {
            return;
        };
        let (posted, failed) = running.posted.unwrap_or_default();
        // As the plugin records it: an event that leaves the review open,
        // with nothing in it, reviewed nothing.
        if posted > 0 || event.submits {
            mode.session.last_reviewed_head = Some(mode.session.head.clone());
        }
        let text = outcome(&event, posted, (refused, failed.saturating_sub(refused)));
        mode.outcome = Some(text.clone());
        // The summary went with the event.
        if event.submits
            && let Some(comments) = &mut self.comments
        {
            match comments.keep_summary("") {
                Ok(changed) => self.placed_again(&changed),
                Err(e) => {
                    self.message(format!("comments: {e}"));
                    return;
                }
            }
        }
        // The link goes in the note only: it would crowd the status bar
        // out of room for the review's part, which a long outcome may not
        // fit in either.
        match self.link.as_deref().filter(|_| failed == 0) {
            Some(url) => self.message(format!("{text} · {url}")),
            None => self.message(text),
        }
    }

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
                self.plan_soon();
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
                    // Said once its threads are in.
                    if let Some(running) = &mut self.job {
                        running.same = true;
                    }
                    return None;
                }
                let moved = (s.head != old.head).then(|| (old.head.clone(), s.head.clone()));
                self.message(format!("{label} updated"));
                self.retarget(comments, moved)
            }
            Job::Post => None,
        }
    }

    /// After an update moved the head, how many commits it moved by, when
    /// git could tell.
    pub(crate) fn review_updated(&mut self, count: Option<usize>) {
        let (Some(mode), Some(count)) = (&self.reviewing, count) else {
            return;
        };
        let note = format!("{} updated · {}", mode.label, plural(count, "new commit"));
        self.message(note);
    }

    fn checked(&mut self, said: Said) {
        let Some(mode) = &mut self.reviewing else {
            return;
        };
        match said {
            Said::Head {
                head,
                moved,
                comments_moved,
            } => {
                mode.moved = (moved && head != mode.session.head).then_some(head);
                mode.comments_moved = comments_moved;
                self.dirty = true;
            }
            Said::Done(result) => {
                mode.check = None;
                mode.next_check = Instant::now() + CHECK_EVERY;
                match result {
                    Ok(()) => mode.check_failed = None,
                    Err(e) if mode.check_failed.as_ref() != Some(&e) => {
                        let note =
                            format!("checking {} failed: {}", mode.label, escape(e.as_bytes()));
                        mode.check_failed = Some(e);
                        self.message(note);
                    }
                    Err(_) => {}
                }
            }
            _ => {}
        }
    }

    /// The review's part of the status bar: `PR #7`, its state when it's
    /// over, what's running, whether the head moved or new comments came,
    /// and what waits on the host.
    pub(super) fn review_part(&self) -> Option<String> {
        let mode = self.reviewing.as_ref()?;
        let mut text = mode.label.clone();
        if let Some(state) = mode.ended() {
            let _ = write!(text, " · {}", state.word());
        }
        if let Some(running) = &self.job {
            let _ = write!(text, " · {}", running.job.doing());
            if let Some(progress) = &running.progress {
                let _ = write!(text, " {progress}");
            }
        } else if mode.moved.is_some() {
            text.push_str(" updated · new commits · p to update");
        } else if mode.comments_moved {
            text.push_str(" · new comments · p to update");
        }
        if let Some(outcome) = &mode.outcome {
            let _ = write!(text, " · {outcome}");
        }
        let pending = self.comments.as_ref().map_or(0, Comments::host_pending);
        if pending > 0 && self.job.is_none() {
            let _ = write!(text, " · {pending} pending · P submits");
        }
        if self.link.is_some() {
            text.push_str(" · o opens it");
        }
        Some(text)
    }

    /// The review's part of the program status: a plugin run is work going
    /// on.
    pub(super) fn review_status(&self) -> Option<term::Status> {
        let running = self.job.as_ref()?;
        let doing = match running.job {
            Job::Prepare => "Fetching threads",
            Job::Pull => "Fetching updates",
            Job::Post => "Posting review",
        };
        // Plugins count progress as done of all: `100/312`.
        let progress = running.progress.as_deref().and_then(|p| {
            let (done, all) = p.split_once('/')?;
            let (done, all) = (done.parse::<u64>().ok()?, all.parse::<u64>().ok()?);
            u8::try_from(done.min(all) * 100 / all.max(1)).ok()
        });
        Some(term::Status {
            state: term::State::Working,
            message: Some(format!("{doing} · {}", running.label)),
            progress,
        })
    }

    /// What the first quit while a post runs says, with how far it got;
    /// `None` while none does.
    pub(super) fn still_posting(&self) -> Option<String> {
        let running = self.job.as_ref().filter(|r| r.job == Job::Post)?;
        let at = running
            .progress
            .as_deref()
            .map(|p| format!(" {}", escape(p.as_bytes())))
            .unwrap_or_default();
        Some(format!("still posting{at} · q again stops it"))
    }

    /// What's left to say once the viewer is gone: drafts in a review
    /// session that weren't posted, `PR #7: 3 drafts not posted`.
    pub(crate) fn unposted_note(&self) -> Option<String> {
        let mode = self.reviewing.as_ref()?;
        let drafts = self.comments.as_ref()?.drafts().len();
        (drafts > 0).then(|| format!("{}: {} not posted", mode.label, plural(drafts, "draft")))
    }

    /// The review session the session pointer names: its comments file,
    /// the review's id and its head.
    pub(crate) fn session_pointer(&self) -> Option<(PathBuf, String, String)> {
        let s = &self.reviewing.as_ref()?.session;
        Some((s.comments.clone(), s.id.clone(), s.head.clone()))
    }

    /// Input that review mode's overlays take, or `o` while a posted review's
    /// note shows; `Continue` when it's for the rest of the viewer.
    pub(super) fn review_input(&mut self, input: &Input) -> ControlFlow<Option<Action>> {
        if self.posting.is_some() {
            return ControlFlow::Break(self.post_input(input));
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

    /// Closes the dialog, keeping its summary in the comments file for
    /// the next time.
    fn close_post(&mut self) -> Option<Dialog> {
        let dialog = self.posting.take()?;
        let kept = self
            .comments
            .as_mut()
            .map(|c| c.keep_summary(dialog.summary.text()));
        match kept {
            Some(Ok(changed)) => self.placed_again(&changed),
            Some(Err(e)) => self.message(format!("the summary isn't kept: {e}")),
            None => {}
        }
        Some(dialog)
    }

    fn post_input(&mut self, input: &Input) -> Option<Action> {
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
            self.close_post();
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
                self.close_post();
            }
            Key::Tab | Key::Char('s') => dialog.writing = true,
            Key::Up | Key::Char('k') => dialog.event = dialog.event.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                dialog.event = (dialog.event + 1).min(dialog.events.len() - 1);
            }
            Key::Char('l') => {
                dialog.local = !dialog.local;
                let local = dialog.local;
                if self.sending(true).local == 0 {
                    self.message("no local drafts on this diff");
                } else {
                    self.message(if local {
                        "the local drafts go with the review"
                    } else {
                        "the local drafts stay local"
                    });
                }
            }
            Key::Char(c) if dialog.events.iter().any(|e| e.key == c) => {
                dialog.event = dialog.events.iter().position(|e| e.key == c).unwrap_or(0);
            }
            Key::Char('v') => {
                let local = dialog.local;
                let left = self.wont_go(local);
                if left.is_empty() {
                    self.message("every draft can be posted");
                } else {
                    self.close_post();
                    self.go_to_unposted(&left);
                }
            }
            Key::Enter => return self.send_review(),
            _ => {}
        }
        None
    }

    /// Posts the review as the dialog has it, once the local drafts in view
    /// are in the review's comments file, where `post` reads them, when
    /// they go. When they can't be moved, nothing is posted and the dialog
    /// stays.
    fn send_review(&mut self) -> Option<Action> {
        let local = self.posting.as_ref()?.local;
        let moved = (self.comments.as_mut())
            .filter(|c| local && c.local_drafts() > 0)
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
        let mut dialog = self.close_post()?;
        let summary = dialog.summary.text().trim_end().to_owned();
        let mode = self.reviewing.as_mut()?;
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
}

impl Sending {
    /// What goes: the drafts, the threads resolved or reopened, and what
    /// waits on the host, `label` naming the review.
    fn what(&self, label: &str) -> String {
        let mut parts = Vec::new();
        if self.comments > 0 {
            parts.push(plural(self.comments, "comment"));
        }
        if self.file > 0 {
            parts.push(format!("{} as a file comment", self.file));
        }
        if let Some(resolves) = self.resolves.filter(|&r| r > 0) {
            parts.push(plural(resolves, "resolve change"));
        }
        if self.pending > 0 {
            parts.push(format!("{} pending on {label}", self.pending));
        }
        if parts.is_empty() {
            "no comments to send".to_owned()
        } else {
            parts.join(" · ")
        }
    }

    /// The one warning: drafts that won't go up as they are.
    fn warning(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.waits > 0 {
            let wait = if self.waits == 1 { "waits" } else { "wait" };
            parts.push(format!("{} {wait} for a push", plural(self.waits, "draft")));
        }
        if self.refused > 0 {
            parts.push(format!("{} refused before", self.refused));
        }
        (!parts.is_empty()).then(|| format!("{} · v goes to them", parts.join(" · ")))
    }
}

impl Dialog {
    /// `checked` says where the plan the counts come from stands.
    pub(super) fn draw(
        &mut self,
        grid: &mut Grid,
        theme: &Theme,
        mode: &Mode,
        (sending, checked): (Sending, Checked),
    ) {
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
        let mut lines = self.heading(theme, mode, (sending, checked));
        lines.push((String::new(), Style::PLAIN));
        let first_event = lines.len();
        for (i, event) in self.events.iter().enumerate() {
            let dot = if i == self.event { "(•)" } else { "( )" };
            lines.push((
                format!("{dot} {}  {}", event.key, event.label),
                Style::PLAIN,
            ));
        }
        lines.push((String::new(), Style::PLAIN));
        let hint = if self.writing {
            "summary · esc back"
        } else {
            "summary · s writes"
        };
        lines.push((hint.to_owned(), dim));
        let summary_at = lines.len();
        let rows = self.summary.rows(width - 6);
        let shown = rows.len().clamp(1, 6);
        let skip = rows
            .iter()
            .position(|(_, c)| c.is_some())
            .map_or(0, |at| (at + 1).saturating_sub(shown));
        let footer = if self.writing {
            "enter new line · esc done"
        } else {
            "enter sends · j/k or the letter picks · esc closes"
        };
        let height = (lines.len() + shown + 4).min(gh);
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        let title = format!(" finish your review · {} ", mode.label);
        help::frame(grid, (x0, y0, width, height), &title, theme);
        let (x, right) = (x0 + 2, x0 + width - 2);
        for (i, (text, style)) in lines.iter().enumerate() {
            let y = y0 + 1 + i;
            if y + 2 >= y0 + height {
                break;
            }
            grid.put_str(x, y, right, text, *style);
            if i.checked_sub(first_event) == Some(self.event) {
                let mut bold = theme.selected(Style::PLAIN, !self.writing);
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
            if let Some(at) = cursor
                && self.writing
            {
                let column = x + 2 + text[..*at].width();
                grid.map_styles(column, y, column + 1, |s| theme.selected(s, true));
                if column >= end {
                    grid.put_str(column, y, right, " ", theme.selected(Style::PLAIN, true));
                }
            }
        }
        grid.put_str(x, y0 + height - 2, right, footer, dim);
    }

    /// The lines over the events: what goes, the one warning about what
    /// won't, the review's state, and the local drafts' toggle.
    fn heading(
        &self,
        theme: &Theme,
        mode: &Mode,
        (sending, checked): (Sending, Checked),
    ) -> Vec<(String, Style)> {
        let (dim, warn) = (theme.style(Role::Dim), theme.style(Role::Warn));
        let mut lines = vec![(sending.what(&mode.label), Style::PLAIN)];
        match (sending.warning(), checked) {
            (Some(warning), _) => lines.push((format!("! {warning}"), warn)),
            (None, Checked::Failed(e)) => lines.push((
                format!(
                    "couldn't check what can be posted: {}",
                    escape(e.as_bytes())
                ),
                dim,
            )),
            (None, Checked::Checking) if sending.comments > 0 => {
                lines.push(("checking what can be posted…".to_owned(), dim));
            }
            (None, _) => {}
        }
        if mode.moved.is_some() {
            lines.push((
                format!(
                    "! {} has new commits; this review goes on the head shown",
                    mode.label
                ),
                warn,
            ));
        }
        if let Some(state) = mode.ended() {
            lines.push((format!("! {} is {}", mode.label, state.word()), warn));
        }
        if sending.local > 0 {
            let mark = if self.local { "[x]" } else { "[ ]" };
            lines.push((
                format!(
                    "l  {mark} with the {} on this diff",
                    plural(sending.local, "local draft")
                ),
                Style::PLAIN,
            ));
        }
        lines
    }

    /// Whether the local drafts in view go with the review.
    pub(super) fn with_local(&self) -> bool {
        self.local
    }
}

impl App {
    /// Where the plan the dialog counts by stands.
    pub(super) fn plan_checked(&self) -> Checked {
        let Some(plans) = self.reviewing.as_ref().map(|m| &m.plans) else {
            return Checked::Checking;
        };
        let waiting = plans.running.is_some() || plans.due.is_some();
        match &plans.failed {
            Some(e) if !waiting => Checked::Failed(e.clone()),
            None if !waiting && plans.known.is_some() => Checked::Yes,
            _ => Checked::Checking,
        }
    }

    fn review_picker_input(&mut self, input: &Input) -> Option<Action> {
        let picker = self.reviews.as_mut()?;
        let last = picker.reviews.len() - 1;
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
                let listed = &picker.reviews[picker.cursor];
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
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 20 || gh < 6 {
            return;
        }
        let mut lines: Vec<Option<usize>> = Vec::new();
        let mut titles: Vec<&str> = Vec::new();
        let mut last = None;
        for (i, &group) in self.group.iter().enumerate() {
            if last != Some(group) {
                if last.is_some() {
                    lines.push(None);
                    titles.push("");
                }
                lines.push(None);
                titles.push(&self.groups[group]);
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
        help::frame(grid, (x0, y0, width, height), &self.title, theme);
        let rows = height - 3;
        let at = lines
            .iter()
            .position(|l| *l == Some(self.cursor))
            .unwrap_or(0);
        if at < self.scroll {
            self.scroll = at.saturating_sub(1);
        } else if at >= self.scroll + rows {
            self.scroll = at + 1 - rows;
        }
        let (x, right) = (x0 + 2, x0 + width - 3);
        let dim = theme.style(Role::Dim);
        for (row, (line, title)) in lines
            .iter()
            .zip(&titles)
            .enumerate()
            .skip(self.scroll)
            .take(rows)
        {
            let y = y0 + 1 + row - self.scroll;
            let Some(i) = *line else {
                let mut bold = theme.style(Role::Plain);
                bold.attributes |= Attributes::BOLD;
                grid.put_str(x, y, right, title, bold);
                continue;
            };
            let listed = &self.reviews[i];
            let selected = i == self.cursor;
            let mark = |s: Style| if selected { theme.selected(s, true) } else { s };
            if selected {
                grid.fill(x - 1, y, right + 1, mark(Style::PLAIN));
            }
            let after = grid.put_str(
                x,
                y,
                right,
                &format!("{:<w$}", self.tags[i], w = self.tag_width),
                mark(theme.style(Role::Hint)),
            );
            let drafts = match listed.drafts {
                0 => String::new(),
                n => format!("  {}", plural(n, "draft")),
            };
            let author = format!("  {}", escape(listed.author.as_bytes()));
            let title_end = right.saturating_sub(author.width() + drafts.width());
            grid.put_str(
                after + 1,
                y,
                title_end,
                &escape(listed.title.as_bytes()),
                mark(Style::PLAIN),
            );
            let end = grid.put_str(title_end, y, right, &author, mark(dim));
            grid.put_str(end, y, right, &drafts, mark(theme.style(Role::Warn)));
        }
        help::scrollbar(
            grid,
            (x0 + width - 2, y0 + 1, rows),
            (lines.len(), self.scroll),
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
