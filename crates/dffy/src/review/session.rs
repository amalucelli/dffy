//! A review session: the review a review plugin prepared, shown as
//! `<base>...<head>` with the review's comments file, and what dffy asks
//! the plugin about it. The plugin runs in the background (see
//! [`crate::plugin::review`]); what a request says comes back for the
//! request's id, and requests no longer wanted are ignored. Every word
//! naming the host's kind of review is the plugin's.
//!
//! [`Reviewing`] is the state machine. A frontend hands it what happened
//! (an intent from a key, the plugin's words, a push, a tick) and carries
//! out the [`Effect`]s it returns, in order: asking the plugin, showing
//! another range, placing threads again, saying something.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dffy_core::bstr::BString;
use dffy_core::git::Revisions;

use super::model::drafts::NO_LINES;
use super::model::reload::{Check, Written};
use super::model::{self, Comments, Hint, Part, Posting};
use crate::plugin::agent;
use crate::plugin::review::{
    self, Listed, Naming, Plan, Push, Reason, Request, ReviewEvent, ReviewLabel, Said, Session,
    State,
};

/// No input for this long makes you idle until the next key, which the
/// review plugin is told: a viewer left open overnight shouldn't have it
/// ask the host all night.
pub(crate) const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long after the drafts change their plan is asked for again, so a
/// burst of edits or a plugin's page of threads asks once.
const PLAN_AFTER: Duration = Duration::from_millis(400);

/// What the frontend does for the review session, in the order given.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Words for the status bar; the last said shows.
    Say(String),
    /// The status bar's words are cleared.
    Hush,
    /// A failure: said, and reported to the terminal until seen.
    Fail(String),
    /// A post ended well, in these words, reported until seen.
    Finished(String),
    /// Ask the plugin `request`; what it says comes back under `id`.
    Ask { id: u64, request: Request },
    /// Ask the agent plugin `request`; what it says comes back under `id`.
    Agent { id: u64, request: agent::Request },
    /// Tell the agent plugin to stop these asks.
    StopAgents(Vec<u64>),
    /// Show `range` when given, and `comments` when it's another file.
    /// `moved` is the review's head before and after, to count commits.
    Show {
        range: Option<Range>,
        comments: Option<PathBuf>,
        moved: Option<(String, String)>,
    },
    /// The words the window title gives what's shown.
    Describe(String),
    /// These paths' comments changed: their threads are placed again.
    Changed(Vec<BString>),
    /// Go to the thread with root `root`, out of the comments panel, then
    /// say `note`.
    Focus { root: String, note: Vec<Part> },
    /// Threads an import brought, by root, in list order: the next thread
    /// is the first of them.
    NewThreads(Vec<String>),
    /// Offer these reviews to open one.
    Pick(Offer),
    /// What's drawn changed.
    Redraw,
}

/// The reviews `list` gave, to open one, grouped: the reasons dffy knows
/// first, then the plugin's groups in the order they came.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Offer {
    pub reviews: Vec<Listed>,
    /// Each review's group, as an index into `groups`.
    pub group: Vec<usize>,
    pub groups: Vec<String>,
    /// Each review's id as shown, after the plugin's prefix.
    pub tags: Vec<String>,
}

impl Offer {
    fn new(reviews: Vec<Listed>, prefix: &str) -> Self {
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
        let tags = (reviews.iter())
            .map(|p| format!("{prefix}{}", p.shown()))
            .collect();
        Self {
            reviews,
            group,
            groups,
            tags,
        }
    }
}

/// A review's range as it stands, and the words for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Range {
    pub revisions: Revisions,
    /// The window title's.
    pub description: String,
    /// The conversation page's.
    pub title: String,
}

/// What plain `dffy` shows, which a review found for the branch keeps.
pub(crate) struct Shown<'a> {
    pub revisions: Option<&'a Revisions>,
    pub description: &'a str,
}

/// The review shown.
pub(crate) struct Mode {
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
    /// The head on the host, when the plugin said it isn't the session's.
    pub moved: Option<String>,
    /// The plugin said the review changed on the host since its comments
    /// were imported: new comments, most likely.
    pub comments_moved: bool,
    /// When the session started, seconds since 1970: local drafts written
    /// since are this review's, and the dialog sends them unasked.
    pub started: u64,
    pub plans: Plans,
}

impl Mode {
    fn new(session: Session, label: String) -> Self {
        Self {
            session,
            label,
            since_last: false,
            plain: None,
            whole: false,
            event: None,
            outcome: None,
            moved: None,
            comments_moved: false,
            started: model::text::now(),
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
    pub(crate) fn ended(&self) -> Option<State> {
        self.session.state.filter(|s| *s != State::Open)
    }
}

/// What `post --dry-run` last said posting would do with each draft. It
/// runs in the background after the drafts change, never while a post
/// does, and its answer goes into the drafts' headings and the dialog.
#[derive(Default)]
pub(crate) struct Plans {
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
pub(crate) enum Checked {
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

/// A plugin run that isn't a `list`, a `detect` or a dry run; one at a
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Job {
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

pub(crate) struct Running {
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

/// What a plugin run is doing, for the terminal's program status.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Work<'a> {
    pub doing: &'static str,
    /// The review it's of, as messages name it.
    pub label: &'a str,
    /// Percent done, when the plugin counts.
    pub progress: Option<u8>,
}

/// What the post dialog starts with: the plugin's events and the one
/// picked, the session's labels each with the value picked by index, the
/// summary kept, and whether the local drafts in view go.
pub(crate) struct Form {
    pub events: Vec<ReviewEvent>,
    pub event: usize,
    pub labels: Vec<(ReviewLabel, usize)>,
    pub summary: String,
    pub local: bool,
}

/// A post as the dialog has it.
pub(crate) struct Post<'a> {
    pub event: ReviewEvent,
    /// The session's labels, each with the value picked, by index: only
    /// those voted otherwise than the host has it are sent.
    pub labels: &'a [(ReviewLabel, usize)],
    pub summary: &'a str,
    /// The local drafts in view go with the review.
    pub local: bool,
}

/// What the dialog says goes, from the drafts and the last plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Sending {
    /// Drafts that go, replies included.
    pub comments: usize,
    /// Of them, on the whole file.
    pub file: usize,
    /// Of them, those the plan skips: they stay drafts.
    pub left_out: usize,
    /// Of them, refused by the host before.
    pub refused: usize,
    /// Local drafts that can go, whether or not they do.
    pub local: usize,
    /// Local drafts that can't, as their lines weren't found: not in
    /// `comments`.
    pub staying: usize,
    /// Threads resolved or reopened and not sent, once a plan said.
    pub resolves: Option<usize>,
    /// Comments in your pending review on the host, which a submitting
    /// event sends.
    pub pending: usize,
}

impl Sending {
    /// What goes: the drafts, the threads resolved or reopened, and what
    /// waits on the host, `label` naming the review.
    pub(crate) fn what(&self, label: &str) -> String {
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

    /// The warnings about drafts that won't go up as they are, a line each;
    /// the last names what goes to them.
    pub(crate) fn warnings(&self) -> Vec<Vec<Part>> {
        let mut parts = Vec::new();
        if self.left_out > 0 {
            let left_out = format!("{} left out", plural(self.left_out, "draft"));
            parts.push(Part::Text(left_out));
        }
        if self.refused > 0 {
            parts.push(Part::Text(format!("{} refused before", self.refused)));
        }
        let mut lines = Vec::new();
        if !parts.is_empty() {
            lines.push(parts);
        }
        if self.staying > 0 {
            let drafts = plural(self.staying, "branch draft");
            let stay = if self.staying == 1 { "stays" } else { "stay" };
            let words = format!("{drafts} {stay}, lines not found");
            lines.push(vec![Part::Text(words)]);
        }
        if let Some(last) = lines.last_mut() {
            last.push(Part::Hint(Hint::GoToUnposted));
        }
        lines
    }
}

/// How a post of `event` ended, in the words the status bar keeps:
/// `review sent`, `review sent · 2 comments refused, kept as drafts`, the
/// event's own words for one that leaves the review open, or `nothing
/// posted`.
fn outcome(event: &ReviewEvent, posted: usize, (refused, left_out): (usize, usize)) -> String {
    let mut text = if event.submits {
        "review sent".to_owned()
    } else if posted + refused + left_out == 0 {
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
    if left_out > 0 {
        let _ = write!(text, " · {} left out", plural(left_out, "draft"));
    }
    text
}

/// Why the plan skips a draft: the plugin's reason, or with none a word
/// that holds for any host.
fn skip_reason(why: &str) -> &str {
    if why.is_empty() {
        "not in the review yet"
    } else {
        why
    }
}

pub(crate) fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// Writes the review's summary to the comments file, as the dialog closes:
/// kept for the next time.
pub(crate) fn keep_summary(comments: Option<&mut Comments>, text: &str) -> Vec<Effect> {
    match comments.map(|c| c.keep_summary(text)) {
        Some(Ok(written)) => read_back(written),
        Some(Err(e)) => vec![Effect::Say(format!("the summary isn't kept: {e}"))],
        None => Vec::new(),
    }
}

/// What a write of dffy's own changed, and why reading it back failed,
/// when it did.
pub(crate) fn read_back(written: Written) -> Vec<Effect> {
    let unread = (written.unread).map(|e| Effect::Say(format!("written, but comments: {e}")));
    std::iter::once(Effect::Changed(written.changed))
        .chain(unread)
        .collect()
}

/// The review plugin's session with dffy: which plugin, its words, the
/// review shown, and the runs going on.
#[derive(Default)]
pub(crate) struct Reviewing {
    /// The review plugin's name, from the config or `--review`, or the one
    /// `auto` found.
    pub plugin: Option<String>,
    /// The review plugin's words, when it describes itself.
    naming: Option<Naming>,
    /// The review shown, in review mode.
    pub mode: Option<Mode>,
    /// The review plugin's `prepare`, `pull` or post, while it runs.
    pub job: Option<Running>,
    /// The last run id handed out.
    run_id: u64,
    /// The `list` run for the review picker, beside any other run.
    listing: Option<u64>,
    /// The `detect` run looking for the branch's review at startup.
    detecting: Option<u64>,
    /// The review just posted, which a frontend offers to open until
    /// another key.
    link: Option<String>,
    /// You're idle, as the review plugin was last told.
    idle: bool,
    /// Whether you're idle, when that changed and the plugin isn't told yet.
    idle_changed: Option<bool>,
    /// When the comments were last read again for a run's writes.
    reloaded: Option<Instant>,
    /// A read held back by [`RELOAD_EVERY`], when it's due and how it
    /// checks the file.
    reload_due: Option<(Instant, Check)>,
}

/// While a plugin runs, what it writes is read back at most this often:
/// each read parses the whole file on the frontend's thread, and an
/// import writes a page at a time, as fast as the host answers.
pub(crate) const RELOAD_EVERY: Duration = Duration::from_millis(250);

impl Reviewing {
    fn next_run(&mut self) -> u64 {
        self.run_id += 1;
        self.run_id
    }

    /// Takes the review plugin's words, as it describes itself.
    pub(crate) fn set_naming(&mut self, naming: Option<Naming>) {
        self.naming = naming;
    }

    pub(crate) fn naming(&self) -> Option<&Naming> {
        self.naming.as_ref()
    }

    /// The review just posted, while it's offered to open.
    pub(crate) fn link(&self) -> Option<&str> {
        self.link.as_deref()
    }

    /// A key other than the one opening it came: the posted review's link
    /// isn't offered any more.
    pub(crate) fn forget_link(&mut self) {
        self.link = None;
    }

    /// Reads the comments again when `check` finds they changed since the
    /// last read, and asks for the drafts' plan again when they changed with
    /// them.
    pub(crate) fn reload(
        &mut self,
        comments: &mut Comments,
        check: Check,
        now: Instant,
    ) -> Vec<Effect> {
        let before = comments.drafts_key();
        match comments.refresh(check) {
            Ok(None) => Vec::new(),
            Ok(Some(changed)) => {
                if self.mode.is_some() && before != comments.drafts_key() {
                    self.plan_soon(now);
                }
                let notice = comments.notice().map(|n| Effect::Say(n.to_owned()));
                std::iter::once(Effect::Changed(changed))
                    .chain(notice)
                    .collect()
            }
            Err(e) => vec![Effect::Say(format!("comments: {e}"))],
        }
    }

    /// The comments file may have changed on disk, as its watch says: read
    /// at once, or while a plugin runs, at [`RELOAD_EVERY`]'s pace, as most
    /// of what changes it then is the run's own writes.
    pub(crate) fn file_changed(&mut self, comments: &mut Comments, now: Instant) -> Vec<Effect> {
        if self.job.is_some() {
            self.reload_soon(Some(comments), Check::Bytes, now)
        } else {
            self.reload_now(Some(comments), Check::Bytes, now)
        }
    }

    /// When a read held back is due.
    pub(crate) fn reload_deadline(&self) -> Option<Instant> {
        self.reload_due.map(|(at, _)| at)
    }

    /// The read held back, once it's due.
    pub(crate) fn due_reload(
        &mut self,
        comments: Option<&mut Comments>,
        now: Instant,
    ) -> Vec<Effect> {
        match self.reload_due {
            Some((at, check)) if now >= at => self.reload_now(comments, check, now),
            _ => Vec::new(),
        }
    }

    fn reload_now(
        &mut self,
        comments: Option<&mut Comments>,
        check: Check,
        now: Instant,
    ) -> Vec<Effect> {
        let check = match self.reload_due.take() {
            Some((_, Check::Bytes)) => Check::Bytes,
            _ => check,
        };
        self.reloaded = Some(now);
        comments.map_or_else(Vec::new, |c| self.reload(c, check, now))
    }

    /// Reads now when the last read is [`RELOAD_EVERY`] old, else once it
    /// will be.
    fn reload_soon(
        &mut self,
        comments: Option<&mut Comments>,
        check: Check,
        now: Instant,
    ) -> Vec<Effect> {
        match self.reloaded.map(|at| at + RELOAD_EVERY) {
            Some(next) if now < next => {
                let check = match (self.reload_due, check) {
                    (Some((_, Check::Bytes)), _) | (_, Check::Bytes) => Check::Bytes,
                    _ => Check::Stat,
                };
                self.reload_due = Some((next, check));
                Vec::new()
            }
            _ => self.reload_now(comments, check, now),
        }
    }

    /// Starts on `session`, whose `prepare` goes on in the background and
    /// says the rest under the returned run id. The comments file shown is
    /// the session's from here: your comments in it not posted yet are
    /// drafts.
    pub(crate) fn start(
        &mut self,
        (plugin, session): (String, Session),
        comments: Option<&mut Comments>,
        now: Instant,
    ) -> (u64, Vec<Effect>) {
        let id = self.next_run();
        let label = self.label_of(session.shown());
        self.job = Some(Running::new(id, Job::Prepare, label.clone()));
        self.plugin = Some(plugin);
        let mode = Mode::new(session, label);
        let mut out = vec![Effect::Describe(mode.description())];
        self.mode = Some(mode);
        let changed = comments.map_or_else(Vec::new, |c| c.set_session(true));
        if !changed.is_empty() {
            out.push(Effect::Changed(changed));
        }
        self.plan_soon(now);
        (id, out)
    }

    /// Offers `reviews`, for the review command (`dffy pr`) on a branch
    /// without one.
    pub(crate) fn pick(&mut self, plugin: String, reviews: Vec<Listed>) -> Vec<Effect> {
        self.plugin = Some(plugin);
        self.offer(reviews)
    }

    fn offer(&self, reviews: Vec<Listed>) -> Vec<Effect> {
        if reviews.is_empty() {
            return vec![Effect::Say(format!(
                "no {} listed for you here",
                self.noun(true)
            ))];
        }
        let prefix = self.naming.as_ref().map_or("", |n| n.id_prefix.as_str());
        vec![Effect::Pick(Offer::new(reviews, prefix))]
    }

    /// The review plugin's word for what it reviews, or the plural;
    /// `review` until a plugin describes itself.
    pub(crate) fn noun(&self, plural: bool) -> &str {
        match (&self.naming, plural) {
            (Some(naming), false) => &naming.noun,
            (Some(naming), true) => &naming.nouns,
            (None, false) => "review",
            (None, true) => "reviews",
        }
    }

    /// How the review whose id shows as `shown` reads in titles and
    /// messages: `PR #7` in the plugin's words.
    pub(crate) fn label_of(&self, shown: &str) -> String {
        match &self.naming {
            Some(naming) => naming.label(shown),
            None => format!("review {shown}"),
        }
    }

    fn not_on_review(&self) -> Effect {
        Effect::Say(match &self.naming {
            Some(n) => format!("not on a {}; dffy {} <id> opens one", n.noun, n.command),
            None => "not on a review".to_owned(),
        })
    }

    /// Whether a post is running: it can't be stopped halfway.
    pub(crate) fn posting_now(&self) -> bool {
        self.job.as_ref().is_some_and(|r| r.job == Job::Post)
    }

    /// The request of `job` on the review `label` names, unless a post is
    /// still running: a post can't be stopped halfway, and anything else
    /// would stop it.
    fn run(&mut self, (job, label): (Job, String), request: Request) -> Vec<Effect> {
        if self.posting_now() {
            return vec![Effect::Say(
                "the review is still being posted; wait for it to finish".to_owned(),
            )];
        }
        let id = self.next_run();
        self.job = Some(Running::new(id, job, label));
        vec![Effect::Ask { id, request }]
    }

    /// What's said when no review plugin is set; `None` when one is.
    fn unset(&self) -> Option<Effect> {
        self.plugin.is_none().then(|| {
            Effect::Say(
                "no review plugin set: review = <name> in the config, or --review <name>"
                    .to_owned(),
            )
        })
    }

    /// "Open review…", in the plugin's words: lists them to pick from,
    /// beside the request going on, which a look at the list shouldn't
    /// stop.
    pub(crate) fn list(&mut self) -> Vec<Effect> {
        if let Some(said) = self.unset() {
            return vec![said];
        }
        let said = Effect::Say(format!("listing {}…", self.noun(true)));
        let id = self.next_run();
        self.listing = Some(id);
        vec![
            said,
            Effect::Ask {
                id,
                request: Request::List,
            },
        ]
    }

    /// Asks `plugin` whether the branch checked out has a review, for
    /// plain `dffy`; the viewer goes on meanwhile.
    pub(crate) fn detect(&mut self, plugin: String) -> (u64, Request) {
        let id = self.next_run();
        self.detecting = Some(id);
        self.plugin = Some(plugin);
        (id, Request::Detect)
    }

    /// What `detect` said: a review for the branch is prepared, then its
    /// comments join plain `dffy`'s range, unless another review was opened
    /// meanwhile.
    fn detected(&mut self, said: Said) -> Vec<Effect> {
        match said {
            Said::Detected(Some(found)) if self.mode.is_none() && self.job.is_none() => {
                if self.plugin.is_none() {
                    return Vec::new();
                }
                let label = self.label_of(found.shown());
                let out = self.run((Job::Prepare, label), Request::Prepare { id: found.id });
                if let Some(running) = &mut self.job {
                    running.local = true;
                }
                out
            }
            Said::Done(result) => {
                self.detecting = None;
                let Err(e) = result else {
                    return Vec::new();
                };
                let noun = self.noun(false);
                vec![Effect::Say(format!(
                    "looking for this branch's {noun} failed: {e}"
                ))]
            }
            _ => Vec::new(),
        }
    }

    /// `p`: fetches the review's head and threads, and shows the new head
    /// in place.
    pub(crate) fn update(&mut self) -> Vec<Effect> {
        let Some(mode) = &self.mode else {
            return vec![self.not_on_review()];
        };
        let (id, label) = (mode.session.id.clone(), mode.label.clone());
        if self.job.as_ref().is_some_and(|r| r.job == Job::Pull) {
            return vec![Effect::Say(format!("already updating {label}"))];
        }
        let mut out = vec![Effect::Say(format!("updating {label}…"))];
        out.extend(self.run((Job::Pull, label), Request::Pull { id }));
        out
    }

    /// `P`: what the dialog that posts the review starts with, the summary
    /// kept from before and the first event that submits, else the one
    /// last sent. It asks for a plan beside whatever else runs, unless the
    /// last still holds.
    pub(crate) fn open_post(&mut self, comments: Option<&Comments>) -> (Option<Form>, Vec<Effect>) {
        let Some(mode) = &self.mode else {
            return (None, vec![self.not_on_review()]);
        };
        if let Some(running) = self.job.as_ref().filter(|r| r.job == Job::Post) {
            let at = running
                .progress
                .as_deref()
                .map(|p| format!(" {p}"))
                .unwrap_or_default();
            return (None, vec![Effect::Say(format!("posting{at}…"))]);
        }
        let events =
            (self.naming.as_ref()).map_or_else(review::default_events, |n| n.events.clone());
        let labels = (mode.session.labels.iter())
            .map(|label| {
                let at = label.values.iter().position(|v| v.value == label.vote);
                (label.clone(), at.unwrap_or(0))
            })
            .collect();
        let last = mode.event.as_ref().map(|e| e.id.as_str());
        let event = events
            .iter()
            .position(|e| e.submits)
            .or_else(|| events.iter().position(|e| Some(e.id.as_str()) == last))
            .unwrap_or(0);
        let (summary, local) = comments.map_or((String::new(), true), |c| {
            (
                c.summary().unwrap_or_default().to_owned(),
                c.local_drafts_before(mode.started) == 0,
            )
        });
        let form = Form {
            events,
            event,
            labels,
            summary,
            local,
        };
        let plans = &mode.plans;
        let out = if plans.running.is_none() && (plans.known.is_none() || plans.due.is_some()) {
            self.start_plan(comments)
        } else {
            Vec::new()
        };
        (Some(form), out)
    }

    /// The drafts' plan, due soon: they changed.
    pub(crate) fn plan_soon(&mut self, now: Instant) {
        if let Some(mode) = &mut self.mode {
            mode.plans.due = Some(now + PLAN_AFTER);
        }
    }

    /// When the plan due should run, while it can: none runs, no post
    /// does, and there are drafts to plan.
    pub(crate) fn plan_deadline(&self, comments: Option<&Comments>) -> Option<Instant> {
        let plans = &self.mode.as_ref()?.plans;
        let due = plans.due?;
        let drafts = || comments.is_some_and(Comments::any_goes);
        (plans.running.is_none() && !self.posting_now() && drafts()).then_some(due)
    }

    /// The plan, once it's due.
    pub(crate) fn due_plan(&mut self, comments: Option<&Comments>, now: Instant) -> Vec<Effect> {
        match self.plan_deadline(comments) {
            Some(due) if now >= due => self.start_plan(comments),
            _ => Vec::new(),
        }
    }

    /// `dryRun`, with the local review's drafts in view, which the plugin
    /// plans after the review's own.
    fn start_plan(&mut self, comments: Option<&Comments>) -> Vec<Effect> {
        let id = self.run_id + 1;
        let drafts = comments.and_then(|c| {
            let roots = c.local_roots();
            Some((c.sources().get(1)?.clone(), roots)).filter(|(_, roots)| !roots.is_empty())
        });
        let Some(mode) = self.mode.as_mut() else {
            return Vec::new();
        };
        self.run_id = id;
        mode.plans.due = None;
        mode.plans.running = Some((id, Answer::default()));
        vec![Effect::Ask {
            id,
            request: Request::DryRun {
                id: mode.session.id.clone(),
                drafts,
            },
        }]
    }

    /// What the dry run says. A failed one says nothing, which the dialog
    /// shows as not knowing; the last answer stays.
    fn planned(&mut self, said: Said, comments: Option<&mut Comments>) -> Vec<Effect> {
        let Some(mode) = &mut self.mode else {
            return Vec::new();
        };
        let Some((_, answer)) = &mut mode.plans.running else {
            return Vec::new();
        };
        let mut out = Vec::new();
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
                let words = (mode.plans.known.iter())
                    .flat_map(|a| &a.drafts)
                    .filter_map(|(id, plan)| {
                        let posting = match plan {
                            Plan::Line => return None,
                            Plan::File => Posting {
                                warn: false,
                                words: "posts as file comment".to_owned(),
                            },
                            Plan::Skip(why) => Posting {
                                warn: true,
                                words: skip_reason(why).to_owned(),
                            },
                        };
                        Some((id.clone(), posting))
                    })
                    .collect();
                let changed = comments.map_or_else(Vec::new, |c| c.set_plans(words));
                if !changed.is_empty() {
                    out.push(Effect::Changed(changed));
                }
            }
            _ => {}
        }
        out.push(Effect::Redraw);
        out
    }

    /// Where the plan the dialog counts by stands.
    pub(crate) fn checked(&self) -> Checked {
        let Some(plans) = self.mode.as_ref().map(|m| &m.plans) else {
            return Checked::Checking;
        };
        let waiting = plans.running.is_some() || plans.due.is_some();
        match &plans.failed {
            Some(e) if !waiting => Checked::Failed(e.clone()),
            None if !waiting && plans.known.is_some() => Checked::Yes,
            _ => Checked::Checking,
        }
    }

    /// What the dialog says goes: the drafts in view, the local ones only
    /// with `local`, counted by what the last plan said of each.
    pub(crate) fn sending(&self, comments: Option<&Comments>, local: bool) -> Sending {
        let Some(comments) = comments else {
            return Sending::default();
        };
        let known = self.mode.as_ref().and_then(|m| m.plans.known.as_ref());
        let mut out = Sending {
            pending: known.map_or_else(|| comments.host_pending(), |k| k.pending),
            resolves: known.map(|k| k.resolves),
            staying: comments.staying().count(),
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
                Some(Plan::Skip(_)) => out.left_out += 1,
                Some(Plan::Line) | None => {}
            }
        }
        out
    }

    /// The drafts that won't go as they are, with why: those the plan
    /// skips, those the host refused before, and the local ones whose lines
    /// weren't found.
    pub(crate) fn wont_go(
        &self,
        comments: Option<&Comments>,
        local: bool,
    ) -> Vec<(String, String)> {
        let (Some(comments), Some(mode)) = (comments, &self.mode) else {
            return Vec::new();
        };
        let known = mode.plans.known.as_ref();
        comments
            .drafts()
            .filter(|&(_, from_local, _)| local || !from_local)
            .filter_map(|(id, _, refused)| {
                let why = match known.and_then(|k| k.drafts.get(id)) {
                    Some(Plan::Skip(why)) => skip_reason(why).to_owned(),
                    _ if refused => "refused before".to_owned(),
                    _ => return None,
                };
                Some((id.to_owned(), why))
            })
            .chain((comments.staying()).map(|id| (id.to_owned(), NO_LINES.to_owned())))
            .collect()
    }

    /// Goes to the thread of the first of `drafts` still in the file,
    /// saying why it won't go; `m`/`M` go on from there.
    pub(crate) fn go_to_unposted(
        comments: Option<&Comments>,
        drafts: &[(String, String)],
    ) -> Effect {
        let found = comments.and_then(|comments| {
            drafts
                .iter()
                .find_map(|d| Some((d, comments.thread_root(&d.0)?)))
        });
        let Some(((_, why), root)) = found else {
            return Effect::Say("the drafts left out aren't in the file any more".to_owned());
        };
        let mut note = vec![Part::Text(format!("not posted: {why}"))];
        if drafts.len() > 1 {
            note.push(Part::Text(format!("{} more", drafts.len() - 1)));
            note.push(Part::Hint(Hint::StepThreads));
        }
        Effect::Focus { root, note }
    }

    /// "Since last review": the range from the head your last review was
    /// on to the head, or back to what showed before.
    pub(crate) fn toggle_since(&mut self) -> Vec<Effect> {
        let whole = format!("the whole {}", self.noun(false));
        let Some(mode) = &mut self.mode else {
            return vec![self.not_on_review()];
        };
        let Some(last) = mode.session.last_reviewed_head.clone() else {
            return vec![Effect::Say(format!(
                "no review of yours posted from dffy on {} yet",
                mode.label
            ))];
        };
        if last == mode.session.head && !mode.since_last {
            return vec![Effect::Say("nothing new since your last review".to_owned())];
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
        let mut out = vec![Effect::Say(note)];
        out.extend(self.retarget(None, None));
        out
    }

    /// "Show whole review": on the review plain `dffy` found, the review's
    /// base against the working tree, or plain `dffy`'s range again.
    pub(crate) fn toggle_whole(&mut self) -> Vec<Effect> {
        let Some(mode) = &mut self.mode else {
            return vec![self.not_on_review()];
        };
        if mode.plain.is_none() {
            return vec![Effect::Say(format!(
                "{} is shown whole already",
                mode.label
            ))];
        }
        mode.whole = !mode.whole;
        mode.since_last = false;
        let note = if mode.whole {
            format!("{} + working tree", mode.label)
        } else {
            "the working tree's changes".to_owned()
        };
        let mut out = vec![Effect::Say(note)];
        out.extend(self.retarget(None, None));
        out
    }

    /// The command panel's words for [`Reviewing::toggle_whole`].
    pub(crate) fn whole_title(&self) -> String {
        if self.mode.as_ref().is_some_and(|p| p.whole) {
            "Show working tree changes".to_owned()
        } else {
            format!("Show whole {}", self.noun(false))
        }
    }

    /// Shows the review's range, as it stands now, and its comments when
    /// `comments` names another file.
    fn retarget(
        &self,
        comments: Option<PathBuf>,
        moved: Option<(String, String)>,
    ) -> Option<Effect> {
        Some(Effect::Show {
            range: Some(self.range()?),
            comments,
            moved,
        })
    }

    /// The review's range as it stands now, which the title and the list
    /// of files take.
    pub(crate) fn range(&self) -> Option<Range> {
        let mode = self.mode.as_ref()?;
        Some(Range {
            revisions: mode.revisions(),
            description: mode.description(),
            title: mode
                .session
                .title
                .as_deref()
                .unwrap_or(&mode.label)
                .to_owned(),
        })
    }

    /// The head of your last review and the head, while "Since last review"
    /// shows from it.
    pub(crate) fn reviewed_from(&self) -> Option<(&str, &str)> {
        let mode = self.mode.as_ref().filter(|p| p.since_last)?;
        Some((
            mode.session.last_reviewed_head.as_deref()?,
            &mode.session.head,
        ))
    }

    /// The head of your last review isn't in the clone, so the range from
    /// it can't be listed: back to the whole review, with what to say.
    pub(crate) fn head_missing(&mut self) -> Option<(String, Range)> {
        let mode = self.mode.as_mut()?;
        mode.since_last = false;
        let last = mode.session.last_reviewed_head.clone().unwrap_or_default();
        let note = format!(
            "last reviewed commit {} isn't in this clone (force-pushed away?)",
            short(&last)
        );
        Some((note, self.range()?))
    }

    /// The review was rebased since your last review: the range from it
    /// takes in what the base gained too, which this says.
    pub(crate) fn rebased_note(&self) -> Option<String> {
        let (last, head) = self.reviewed_from()?;
        Some(format!(
            "rebased since your last review; showing {}..{}, the base's changes since included",
            short(last),
            short(head)
        ))
    }

    /// Whether the range shown is plain `dffy`'s: on no review, or on the
    /// one found for the branch while it keeps plain `dffy`'s range.
    pub(crate) fn shows_plain(&self) -> bool {
        (self.mode.as_ref()).is_none_or(|m| m.plain.is_some() && !m.whole && !m.since_last)
    }

    /// Whether a review is open: what pulling, posting and "Since last
    /// review" act on, and refuse without.
    pub(crate) fn on_review(&self) -> bool {
        self.mode.is_some()
    }

    /// Whether the review open is the one plain `dffy` found, which "Show
    /// whole review" switches; it refuses on any other.
    pub(crate) fn found_plain(&self) -> bool {
        self.mode.as_ref().is_some_and(|m| m.plain.is_some())
    }

    pub(crate) fn since_review(&self) -> bool {
        self.mode.as_ref().is_some_and(|p| p.since_last)
    }

    /// When you'll be idle, on a review, unless you are already, after the
    /// last input at `last_input`.
    pub(crate) fn idle_deadline(&self, last_input: Instant) -> Option<Instant> {
        self.mode.as_ref()?;
        (!self.idle).then(|| last_input + IDLE_AFTER)
    }

    /// You're idle once the deadline passed.
    pub(crate) fn went_idle(&mut self, last_input: Instant, now: Instant) {
        if self.idle_deadline(last_input).is_some_and(|at| now >= at) {
            self.idle = true;
            self.idle_changed = Some(true);
        }
    }

    /// Input came: you're not idle.
    pub(crate) fn input(&mut self) {
        if std::mem::take(&mut self.idle) {
            self.idle_changed = Some(false);
        }
    }

    /// Whether you're idle, when that changed since the last call: for the
    /// review plugin.
    pub(crate) fn take_idle(&mut self) -> Option<bool> {
        self.idle_changed.take()
    }

    /// What the plugin pushed, about the review shown or not at all.
    pub(crate) fn pushed(&mut self, push: Push) -> Vec<Effect> {
        let mode = self.mode.as_mut();
        match push {
            Push::HeadMoved { id, head } => {
                if let Some(mode) = mode.filter(|m| m.session.id == id) {
                    mode.moved = (head != mode.session.head).then_some(head);
                    return vec![Effect::Redraw];
                }
            }
            Push::CommentsChanged { id } => {
                if let Some(mode) = mode.filter(|m| m.session.id == id) {
                    mode.comments_moved = true;
                    return vec![Effect::Redraw];
                }
            }
            Push::Log(said) => return vec![Effect::Say(said)],
        }
        Vec::new()
    }

    /// Before a `prepare`'s or `pull`'s word is read: the threads the
    /// comments file has, to tell the ones it brings. Nothing to tell them
    /// from on the first word of a `prepare` of a review whose file isn't
    /// shown yet, as when it was just picked or opened for the first time.
    fn before_import(&mut self, id: u64, comments: Option<&Comments>) {
        let shown = comments
            .filter(|c| {
                self.mode
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

    /// What the plugin said under run `id`; requests no longer wanted are
    /// ignored. `shown` is what plain `dffy` shows, which a review found
    /// for the branch keeps.
    ///
    /// A plugin's operations are in the comments file before what follows
    /// them comes, so the file is read again first (dffy wrote it,
    /// replacing it, so its stat tells), as what's said may look the
    /// threads up: at once for a session and an end, at [`RELOAD_EVERY`]'s
    /// pace for progress.
    pub(crate) fn said(
        &mut self,
        (id, said): (u64, Said),
        mut comments: Option<&mut Comments>,
        shown: &Shown,
        now: Instant,
    ) -> Vec<Effect> {
        self.before_import(id, comments.as_deref());
        let mut out = match said {
            Said::Session(_) | Said::Done(_) => {
                self.reload_now(comments.as_deref_mut(), Check::Stat, now)
            }
            Said::Progress(_) => self.reload_soon(comments.as_deref_mut(), Check::Stat, now),
            _ => Vec::new(),
        };
        out.extend(self.heard((id, said), comments, shown, now));
        out
    }

    fn heard(
        &mut self,
        (id, said): (u64, Said),
        mut comments: Option<&mut Comments>,
        shown: &Shown,
        now: Instant,
    ) -> Vec<Effect> {
        if self.listing == Some(id) {
            return self.listed(said);
        }
        if self.detecting == Some(id) {
            return self.detected(said);
        }
        if (self.mode.as_ref())
            .and_then(|p| p.plans.running.as_ref())
            .is_some_and(|(run, _)| *run == id)
        {
            return self.planned(said, comments);
        }
        let Some(running) = self.job.as_mut().filter(|r| r.id == id) else {
            return Vec::new();
        };
        let (job, local) = (running.job, running.local);
        let mut out = vec![Effect::Redraw];
        match said {
            Said::Progress(progress) => running.progress = Some(progress),
            Said::Session(session) => out.extend(self.prepared((job, local), session, shown, now)),
            Said::Draft(draft) => {
                if let Plan::Skip(why) = draft.plan {
                    running
                        .unposted
                        .push((draft.id, skip_reason(&why).to_owned()));
                }
            }
            Said::Posted {
                posted,
                failed,
                url,
                last_reviewed_head,
                labels,
                ..
            } => {
                running.posted = Some((posted, failed));
                self.link = url.filter(|u| model::text::is_web(u));
                // The plugin's to record: it says what it recorded.
                if let Some(mode) = &mut self.mode {
                    if let Some(head) = last_reviewed_head {
                        mode.session.last_reviewed_head = Some(head);
                    }
                    if let Some(labels) = labels {
                        mode.session.labels = labels;
                    }
                }
            }
            Said::Done(result) => {
                let Some(running) = self.job.take() else {
                    return out;
                };
                self.plan_soon(now);
                match (job, result) {
                    (Job::Post, Ok(())) => {
                        out.extend(self.posted(&running, comments.as_deref_mut()));
                        // The plugin's notice, read as the run ended, says
                        // more than the outcome, which the status bar keeps.
                        if let Some(notice) = comments.as_deref().and_then(Comments::notice) {
                            out.push(Effect::Say(notice.to_owned()));
                        }
                        out.extend(self.mode.as_ref().and_then(|mode| {
                            let mut outcome = mode.outcome.as_ref()?.chars();
                            let first = outcome.next()?.to_uppercase();
                            Some(Effect::Finished(format!(
                                "{first}{} · {}",
                                outcome.as_str(),
                                mode.label
                            )))
                        }));
                    }
                    (Job::Prepare | Job::Pull, Ok(())) => {
                        out.extend(self.imported(&running, comments.as_deref()));
                    }
                    (job, Err(e)) => {
                        let label = &running.label;
                        out.push(Effect::Fail(match job {
                            Job::Prepare => format!("{label}: {e}"),
                            Job::Pull => format!("updating {label} failed: {e}"),
                            Job::Post => format!("posting failed: {e}"),
                        }));
                    }
                }
            }
            Said::Listed(_) | Said::Detected(_) | Said::Planned { .. } | Said::Checks(_) => {}
        }
        out
    }

    /// A `prepare` or `pull` finished: the threads it brought, which `m`
    /// goes to first, or that the review is as it was.
    fn imported(&mut self, running: &Running, comments: Option<&Comments>) -> Vec<Effect> {
        if let Some(mode) = &mut self.mode {
            mode.comments_moved = false;
        }
        let new: HashSet<String> = match (&running.before, comments) {
            (Some(before), Some(comments)) => comments
                .posted_roots()
                .into_iter()
                .filter(|(reference, _)| !before.contains_key(reference))
                .map(|(_, id)| id)
                .collect(),
            _ => HashSet::new(),
        };
        if let Some(comments) = comments.filter(|_| !new.is_empty()) {
            vec![Effect::NewThreads(comments.in_order(&new))]
        } else if running.same {
            vec![Effect::Say(format!("{} is up to date", running.label))]
        } else {
            Vec::new()
        }
    }

    /// A post that finished: what it did goes in the status bar. Drafts it
    /// left out stay drafts; one the host refused says why in its heading.
    fn posted(&mut self, running: &Running, comments: Option<&mut Comments>) -> Vec<Effect> {
        let refused = comments.as_deref().map_or(0, |c| {
            running
                .unposted
                .iter()
                .filter(|(id, _)| c.refused(id))
                .count()
        });
        let Some(mode) = &mut self.mode else {
            return Vec::new();
        };
        let Some(event) = mode.event.clone() else {
            return Vec::new();
        };
        let (posted, failed) = running.posted.unwrap_or_default();
        let text = outcome(&event, posted, (refused, failed.saturating_sub(refused)));
        mode.outcome = Some(text.clone());
        let mut out = Vec::new();
        // The summary went with the event.
        if event.submits
            && let Some(comments) = comments
        {
            match comments.keep_summary("") {
                Ok(written) => out.extend(read_back(written)),
                Err(e) => {
                    out.push(Effect::Say(format!("comments: {e}")));
                    return out;
                }
            }
        }
        // The link goes in the note only: it would crowd the status bar
        // out of room for the review's part, which a long outcome may not
        // fit in either.
        out.push(Effect::Say(
            match self.link.as_deref().filter(|_| failed == 0) {
                Some(url) => format!("{text} · {url}"),
                None => text,
            },
        ));
        out
    }

    fn listed(&mut self, said: Said) -> Vec<Effect> {
        let mut out = match said {
            Said::Listed(reviews) => {
                let mut out = vec![Effect::Hush];
                out.extend(self.offer(reviews));
                out
            }
            Said::Done(result) => {
                self.listing = None;
                match result {
                    Ok(()) => Vec::new(),
                    Err(e) => vec![Effect::Say(format!(
                        "listing {} failed: {e}",
                        self.noun(true)
                    ))],
                }
            }
            _ => Vec::new(),
        };
        out.push(Effect::Redraw);
        out
    }

    /// A session line: a review opened from the picker or found for the
    /// branch (`local`), or the review updated.
    fn prepared(
        &mut self,
        (job, local): (Job, bool),
        session: Session,
        shown: &Shown,
        now: Instant,
    ) -> Vec<Effect> {
        let label = self.label_of(session.shown());
        match job {
            Job::Prepare => {
                let comments = session.comments.clone();
                let title = session.title.clone().unwrap_or_default();
                let mut mode = Mode::new(session, label.clone());
                if local {
                    let revisions = shown.revisions.cloned().unwrap_or_default();
                    mode.plain = Some((revisions, shown.description.to_owned()));
                }
                // The retarget takes the session's comments file, read as
                // a session's from here on.
                self.mode = Some(mode);
                self.plan_soon(now);
                let on = if local { " · this branch's" } else { "" };
                let mut out = vec![Effect::Say(format!("{label}{on} · {title}"))];
                if local {
                    // The range stays plain dffy's: only the comments change.
                    out.push(Effect::Show {
                        range: None,
                        comments: Some(comments),
                        moved: None,
                    });
                    return out;
                }
                out.extend(self.retarget(Some(comments), None));
                out
            }
            Job::Pull => {
                let Some(mode) = self.mode.as_mut() else {
                    return Vec::new();
                };
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
                    return Vec::new();
                }
                let moved = (s.head != old.head).then(|| (old.head.clone(), s.head.clone()));
                let mut out = vec![Effect::Say(format!("{label} updated"))];
                out.extend(self.retarget(comments, moved));
                out
            }
            Job::Post => Vec::new(),
        }
    }

    /// After an update moved the head, how many commits it moved by, when
    /// git could tell.
    pub(crate) fn updated(&self, count: Option<usize>) -> Vec<Effect> {
        let (Some(mode), Some(count)) = (&self.mode, count) else {
            return Vec::new();
        };
        vec![Effect::Say(format!(
            "{} updated · {}",
            mode.label,
            plural(count, "new commit")
        ))]
    }

    /// The review's part of the status bar: `PR #7`, its state when it's
    /// over, what's running, whether the head moved or new comments came,
    /// and what waits on the host.
    pub(crate) fn status(&self, comments: Option<&Comments>) -> Option<Vec<Part>> {
        let mode = self.mode.as_ref()?;
        let mut words = vec![Part::Text(mode.label.clone())];
        if let Some(state) = mode.ended() {
            words.push(Part::Text(state.word().to_owned()));
        }
        if let Some(running) = &self.job {
            let mut doing = running.job.doing().to_owned();
            if let Some(progress) = &running.progress {
                let _ = write!(doing, " {progress}");
            }
            words.push(Part::Text(doing));
        } else if mode.moved.is_some() {
            if let Some(Part::Text(last)) = words.last_mut() {
                last.push_str(" updated");
            }
            words.push(Part::Text("new commits".to_owned()));
            words.push(Part::Hint(Hint::Update));
        } else if mode.comments_moved {
            words.push(Part::Text("new comments".to_owned()));
            words.push(Part::Hint(Hint::Update));
        }
        if let Some(outcome) = &mode.outcome {
            words.push(Part::Text(outcome.clone()));
        }
        let pending = comments.map_or(0, Comments::host_pending);
        if pending > 0 && self.job.is_none() {
            words.push(Part::Text(format!("{pending} pending")));
            words.push(Part::Hint(Hint::Submit));
        }
        if self.link.is_some() {
            words.push(Part::Hint(Hint::OpenPosted));
        }
        Some(words)
    }

    /// The plugin run going on, as work for the program status.
    pub(crate) fn work(&self) -> Option<Work<'_>> {
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
        Some(Work {
            doing,
            label: &running.label,
            progress,
        })
    }

    /// What the first quit while a post runs says, with how far it got;
    /// `None` while none does.
    pub(crate) fn still_posting(&self) -> Option<Vec<Part>> {
        let running = self.job.as_ref().filter(|r| r.job == Job::Post)?;
        let at = running
            .progress
            .as_deref()
            .map(|p| format!(" {p}"))
            .unwrap_or_default();
        Some(vec![
            Part::Text(format!("still posting{at}")),
            Part::Hint(Hint::StopPosting),
        ])
    }

    /// What's left to say once the viewer is gone: drafts in a review
    /// session that weren't posted, `PR #7: 3 drafts not posted`.
    pub(crate) fn unposted_note(&self, comments: Option<&Comments>) -> Option<String> {
        let mode = self.mode.as_ref()?;
        let (_, drafts) = comments?.counts();
        (drafts > 0).then(|| format!("{}: {} not posted", mode.label, plural(drafts, "draft")))
    }

    /// The review session the session pointer names: its comments file,
    /// the review's id and its head.
    pub(crate) fn session_pointer(&self) -> Option<(PathBuf, String, String)> {
        let s = &self.mode.as_ref()?.session;
        Some((s.comments.clone(), s.id.clone(), s.head.clone()))
    }

    /// Opens review `id`, `label` naming it, picked from the list.
    pub(crate) fn open(&mut self, id: String, label: String) -> Vec<Effect> {
        if self.mode.as_ref().is_some_and(|p| p.session.id == id) {
            return vec![Effect::Say(format!("{label} is open"))];
        }
        if let Some(said) = self.unset() {
            return vec![said];
        }
        let mut out = vec![Effect::Say(format!("fetching {label}…"))];
        out.extend(self.run((Job::Prepare, label), Request::Prepare { id }));
        out
    }

    /// Posts the review as the dialog has it, once the local drafts in view
    /// are in the review's comments file, where `post` reads them, when
    /// they go; the summary is kept as the dialog closes. While a `prepare`
    /// or a `pull` runs, or when the drafts can't be moved, nothing is
    /// posted and the dialog stays: `Err`. The plugin takes requests side by
    /// side, so a post beside an import would race it for the drafts.
    pub(crate) fn send(
        &mut self,
        mut comments: Option<&mut Comments>,
        post: Post,
    ) -> Result<Vec<Effect>, Vec<Effect>> {
        if let Some(running) = &self.job {
            return Err(vec![Effect::Say(format!(
                "still {}; post once it's done",
                running.job.doing()
            ))]);
        }
        let mut out = Vec::new();
        let moved = (comments.as_deref_mut())
            .filter(|c| post.local && c.local_drafts() > 0)
            .map(Comments::move_local);
        match moved {
            Some(Ok(written)) => out.extend(read_back(written)),
            Some(Err(e)) => {
                let noun = self.noun(false);
                return Err(vec![Effect::Say(format!(
                    "nothing posted: branch drafts not moved to the {noun}: {e}"
                ))]);
            }
            None => {}
        }
        out.extend(keep_summary(comments, post.summary));
        let summary = post.summary.trim_end().to_owned();
        let Some(mode) = self.mode.as_mut() else {
            return Ok(out);
        };
        let (id, label) = (mode.session.id.clone(), mode.label.clone());
        let labels = (post.labels.iter())
            .map(|(label, at)| (label, label.values[*at].value))
            .filter(|(label, value)| *value != label.vote)
            .map(|(label, value)| (label.name.clone(), value))
            .collect();
        let request = Request::Post {
            id,
            event: post.event.id.clone(),
            labels,
            summary: (!summary.is_empty()).then_some(summary),
        };
        mode.event = Some(post.event);
        mode.outcome = None;
        out.push(Effect::Say(format!("posting the review on {label}…")));
        out.extend(self.run((Job::Post, label), request));
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
