//! Review plugins (experimental): programs named `dffy-review-<name>` that
//! keep the comments file in step with a code host, since dffy never
//! touches the network. A plugin runs as one long-lived process per
//! repository, a [`Plugin`], speaking JSON-RPC 2.0 over its stdin and
//! stdout through an [`rpc::Client`]; docs/plugins.md has the messages.

use std::io;
#[cfg(test)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dffy_core::cache::Cache;
#[cfg(test)]
use rustix::process::Pid;

use super::Program;
use super::json::{self, Value};
use super::rpc::{self, Pending};

/// The protocol version, sent in `initialize` and named in its answer; a
/// plugin of another version is refused.
pub(crate) const PROTOCOL: usize = 1;

/// How long the plugin's `doctor` may take: its checks may ask the host.
/// Past it, its checks say it didn't answer, and `dffy doctor` goes on.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(3);

/// What a plugin's `initialize` says: the subcommand dffy adds for it, the
/// words for what it reviews, which dffy's screens use, and the events the
/// post dialog offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Naming {
    /// `pr`, for `dffy pr [<id>]`.
    pub command: String,
    /// `pull request`.
    pub noun: String,
    /// `pull requests`.
    pub nouns: String,
    /// `PR`, before a review's id: `PR #7`. The noun when not described.
    pub short: String,
    /// `#`, right before the id; may be empty.
    pub id_prefix: String,
    /// The post dialog's choices, in order; never empty.
    pub events: Vec<ReviewEvent>,
    /// Said the first time you reply to an outdated thread; none, nothing.
    pub outdated_reply_warning: Option<String>,
}

/// A label a review is voted on, as Gerrit's Code-Review or Verified, as a
/// session has it: which labels a review has, and your vote on each, are
/// the review's, and change with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewLabel {
    /// Sent as a key of `post`'s `labels`.
    pub name: String,
    /// In the order the dialog offers them; 1 to 9, none repeated.
    pub values: Vec<LabelValue>,
    /// Your vote on the host now, one of `values`, which the dialog starts
    /// on; `post` sends a label only when it's voted otherwise.
    pub vote: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelValue {
    pub value: i32,
    /// Shown beside it: `Looks good to me`.
    pub label: String,
}

/// One way `post` can end a review, as the plugin names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewEvent {
    /// Sent as `post`'s `event`.
    pub id: String,
    /// Picks it in the dialog.
    pub key: char,
    pub label: String,
    /// The status bar's words once it went: `approved`.
    pub done: String,
    /// Sending it ends the review on the host, sending what's pending there
    /// with it; a `false` event leaves the review open there.
    pub submits: bool,
}

/// The dialog's own keys, which an event's key can't be.
const DIALOG_KEYS: [char; 5] = ['j', 'k', 'l', 's', 'v'];

impl Naming {
    /// Whether `word` has the shape of a command a plugin may describe.
    pub(crate) fn can_be_command(word: &str) -> bool {
        word.starts_with(|c: char| c.is_ascii_lowercase())
            && word
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    /// Reads the naming of an `initialize` answer. A command is lowercase
    /// letters, digits and `-`, starting with a letter. Words are one line
    /// of printable text, since help and the viewer print them as they are.
    pub(crate) fn decode(value: &Value) -> Result<Self, String> {
        let field =
            |key: &str| text(value, key).ok_or_else(|| format!("its initialize has no {key}"));
        let command = field("command")?;
        if !Self::can_be_command(&command) {
            return Err(format!(
                "its command '{}' isn't lowercase letters, digits and -, starting with a letter",
                command.escape_debug()
            ));
        }
        let noun = noun("noun", field("noun")?, 40)?;
        let short = match text(value, "short") {
            Some(short) => self::noun("short", short, 40)?,
            None => noun.clone(),
        };
        let id_prefix = text(value, "id_prefix").unwrap_or_default();
        if id_prefix.chars().count() > 8 || id_prefix.contains(char::is_control) {
            return Err(format!(
                "its id_prefix '{}' isn't up to 8 printable characters",
                id_prefix.escape_debug()
            ));
        }
        let events = match value.get("events") {
            None | Some(Value::Null) => default_events(),
            Some(events) => review_events(events)?,
        };
        let outdated_reply_warning = text(value, "outdated_reply_warning")
            .map(|w| self::noun("outdated_reply_warning", w, 200))
            .transpose()?;
        Ok(Self {
            nouns: self::noun("nouns", field("nouns")?, 40)?,
            noun,
            command,
            short,
            id_prefix,
            events,
            outdated_reply_warning,
        })
    }

    /// How a review reads in titles and messages: `PR #7`.
    pub(crate) fn label(&self, shown: &str) -> String {
        format!("{} {}{shown}", self.short, self.id_prefix)
    }
}

/// An `initialize` answer: the naming, and what the plugin does beyond the
/// requests every plugin answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Init {
    naming: Naming,
    /// It pushes `head/moved` and `comments/changed` for the review dffy
    /// says it shows, so dffy tells it which, and when you're idle.
    watch: bool,
}

impl Init {
    fn decode(result: &Value) -> Result<Self, String> {
        match result.get("protocol").map(Value::as_usize) {
            Some(Some(PROTOCOL)) => {}
            Some(Some(other)) => {
                return Err(format!(
                    "it speaks review protocol {other}, and dffy speaks {PROTOCOL}"
                ));
            }
            _ => return Err("its initialize names no protocol".to_owned()),
        }
        let watch = result
            .get("capabilities")
            .and_then(|c| c.get("watch"))
            .is_some_and(|w| *w == Value::Bool(true));
        Ok(Self {
            naming: Naming::decode(result)?,
            watch,
        })
    }
}

/// The events of a plugin that names none: `post`.
pub(crate) fn default_events() -> Vec<ReviewEvent> {
    vec![ReviewEvent {
        id: "post".to_owned(),
        key: 'p',
        label: "post".to_owned(),
        done: "posted".to_owned(),
        submits: true,
    }]
}

/// `text`, when it's 1 to `most` printable characters.
fn noun(key: &str, text: String, most: usize) -> Result<String, String> {
    if text.trim().is_empty() || text.chars().count() > most || text.contains(char::is_control) {
        return Err(format!(
            "its {key} '{}' isn't 1 to {most} printable characters",
            text.escape_debug()
        ));
    }
    Ok(text)
}

/// The `events` of an `initialize` answer: 1 to 9, each id a word and each
/// key one letter or digit the dialog doesn't take, neither repeated.
fn review_events(events: &Value) -> Result<Vec<ReviewEvent>, String> {
    let events = events.as_array().ok_or("its events isn't a list")?;
    if events.is_empty() || events.len() > 9 {
        return Err("its events aren't 1 to 9".to_owned());
    }
    let mut out: Vec<ReviewEvent> = Vec::with_capacity(events.len());
    for event in events {
        let id = text(event, "id").unwrap_or_default();
        if !id.starts_with(|c: char| c.is_ascii_lowercase())
            || id.len() > 40
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(format!(
                "its event '{}' isn't lowercase letters, digits, _ and -, starting with a letter",
                id.escape_debug()
            ));
        }
        let key = text(event, "key").unwrap_or_default();
        let key = match key.chars().collect::<Vec<_>>()[..] {
            [k] if (k.is_ascii_lowercase() || k.is_ascii_digit()) && !DIALOG_KEYS.contains(&k) => k,
            _ => {
                return Err(format!(
                    "its event {id}'s key '{}' isn't one letter or digit other than j, k, l, s and v",
                    key.escape_debug()
                ));
            }
        };
        if out.iter().any(|e| e.id == id || e.key == key) {
            return Err(format!("its event {id} repeats an id or a key"));
        }
        let words = |field: &str| {
            let said = text(event, field).ok_or_else(|| format!("its event {id} has no {field}"));
            noun(&format!("event {id}'s {field}"), said?, 40)
        };
        out.push(ReviewEvent {
            label: words("label")?,
            done: words("done")?,
            submits: event.get("submits") != Some(&Value::Bool(false)),
            key,
            id,
        });
    }
    Ok(out)
}

/// The `labels` of a session: up to 9, each a name of its own with 1 to 9
/// whole-number values, none repeated, and your vote among them. A name is
/// printable rather than a word, as `Code-Review` is.
fn review_labels(labels: &Value) -> Result<Vec<ReviewLabel>, String> {
    let labels = labels.as_array().ok_or("its labels isn't a list")?;
    if labels.len() > 9 {
        return Err("its labels are more than 9".to_owned());
    }
    // Through its decimal text, which a fraction or a number past i32
    // doesn't parse from.
    let whole = |value: Option<&Value>| match value {
        Some(Value::Number(n)) => n.to_string().parse::<i32>().ok(),
        _ => None,
    };
    let mut out: Vec<ReviewLabel> = Vec::with_capacity(labels.len());
    for label in labels {
        let name = noun("label", text(label, "name").unwrap_or_default(), 40)?;
        if out.iter().any(|l| l.name == name) {
            return Err(format!("its label {name} repeats a name"));
        }
        let given = label
            .get("values")
            .and_then(Value::as_array)
            .unwrap_or_default();
        if given.is_empty() || given.len() > 9 {
            return Err(format!("its label {name} doesn't have 1 to 9 values"));
        }
        let mut values: Vec<LabelValue> = Vec::with_capacity(given.len());
        for v in given {
            let value = whole(v.get("value"))
                .ok_or_else(|| format!("a value of its label {name} isn't a whole number"))?;
            if values.iter().any(|v| v.value == value) {
                return Err(format!("its label {name} repeats a value"));
            }
            let words = text(v, "label").unwrap_or_default();
            values.push(LabelValue {
                value,
                label: noun(&format!("label {name}'s value {value}"), words, 60)?,
            });
        }
        let vote = whole(label.get("vote"))
            .filter(|d| values.iter().any(|v| v.value == *d))
            .ok_or_else(|| format!("its label {name}'s vote isn't one of its values"))?;
        out.push(ReviewLabel { name, values, vote });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub(crate) struct Described {
    pub program: Program,
    pub naming: Naming,
}

/// `program`'s naming, from its `initialize` answer as [`rpc::describe`]
/// keeps it. A command `reserved` says is dffy's own is refused, cached
/// or not.
pub(crate) fn describe(
    program: &Program,
    cache: Option<&Cache>,
    fresh: bool,
    reserved: impl Fn(&str) -> bool,
) -> Result<Naming, String> {
    let decode = |result: &Value| Init::decode(result).map(|i| i.naming);
    let naming = rpc::describe((program, PROTOCOL), (cache, fresh), decode, || {
        Plugin::start(program, (None, None), cache.cloned())
            .map_err(|e| format!("couldn't start it: {e}"))?
            .0
            .wait_initialized(|init| init.naming.clone())
    })?;
    if reserved(&naming.command) {
        return Err(format!(
            "its command '{}' is one of dffy's own",
            naming.command
        ));
    }
    Ok(naming)
}

/// One of the plugin's own checks, from its `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// The few words `dffy doctor`'s row of the check's group gives it.
    pub brief: Option<String>,
    /// A passed check that only notes something, as an agent that isn't
    /// installed: neither a pass nor a problem.
    pub info: bool,
}

/// The plugin's checks of what it needs, for `dffy doctor`: a session of
/// its own, never cached.
pub(crate) fn doctor(program: &Program) -> Result<Vec<Check>, String> {
    let plugin = Plugin::start(program, (None, None), None)
        .map_err(|e| format!("couldn't start it: {e}"))?;
    let (tx, rx) = mpsc::channel();
    plugin.request(&Request::Doctor, move |said| tx.send(said).is_ok());
    let until = Instant::now() + DOCTOR_TIMEOUT;
    let checks = loop {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Said::Checks(checks)) => break Ok(checks),
            Ok(Said::Done(Err(e))) => break Err(e),
            Ok(Said::Done(Ok(()))) => break Err("its doctor has no checks".to_owned()),
            Ok(_) => {}
            Err(_) => {
                break Err(format!(
                    "its doctor didn't answer in {}s",
                    DOCTOR_TIMEOUT.as_secs()
                ));
            }
        }
    };
    plugin.close();
    checks
}

/// A review, from a `session` notification or `detect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    /// The plugin's id, which its requests take.
    pub id: String,
    /// What to show for the id, when the plugin gives one.
    pub label: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    /// Commits in the clone: dffy shows `<base>...<head>`.
    pub base: String,
    pub head: String,
    /// The head of the last review posted, if any.
    pub last_reviewed_head: Option<String>,
    /// As the host last said; `None` when the plugin didn't say, or said
    /// something dffy doesn't know.
    pub state: Option<State>,
    /// What the post dialog offers a vote on beside the event, each a
    /// choice of its own; none for a host whose verdict is the event.
    pub labels: Vec<ReviewLabel>,
    /// The comments file, absolute.
    pub comments: PathBuf,
    /// The command that checks the review out, as the plugin says it, for
    /// dffy to show where something needs HEAD on the review's head.
    pub checkout: Option<String>,
}

impl Session {
    pub(crate) fn shown(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }
}

/// Where a review stands on its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Open,
    Closed,
    Merged,
}

impl State {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }
}

/// Why `list` lists a review; the picker groups by it, these first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    CurrentBranch,
    ReviewRequested,
    Mine,
    /// A reason this dffy doesn't know: grouped by the entry's `group`.
    Other,
}

/// A review from `list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub id: String,
    pub label: Option<String>,
    pub title: String,
    pub author: String,
    pub reason: Reason,
    /// The plugin's title for its group, for a reason dffy doesn't know.
    pub group: Option<String>,
    /// Your drafts its comments file holds.
    pub drafts: usize,
}

impl Listed {
    pub(crate) fn shown(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }
}

/// What `post` does, or did, with a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// On its lines, or a reply in its thread.
    Line,
    /// On the whole file.
    File,
    /// Left a draft, with why.
    Skip(String),
}

/// What `dryRun` plans for a draft, or a draft a `post` left unposted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Draft {
    pub id: String,
    pub path: String,
    pub plan: Plan,
}

/// What a request says, in order; [`Said::Done`] is last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Said {
    Session(Session),
    /// `detect`: the branch checked out's review, as a session, if it has
    /// one.
    Detected(Option<Session>),
    Progress(String),
    Listed(Vec<Listed>),
    Draft(Draft),
    /// `dryRun` finished: how many drafts go on a line, on the file, and
    /// nowhere; how many comments wait in your pending review on the host;
    /// how many threads were resolved or reopened and not sent.
    Planned {
        line: usize,
        file: usize,
        skip: usize,
        pending: usize,
        resolves: usize,
    },
    /// `post` finished: comments posted and not, how many your pending
    /// review on the host had before, the review's link, the head the
    /// plugin recorded as last reviewed, when it did, and the review's
    /// labels with your votes as the post left them, when it said.
    Posted {
        posted: usize,
        failed: usize,
        pending: usize,
        url: Option<String>,
        last_reviewed_head: Option<String>,
        labels: Option<Vec<ReviewLabel>>,
    },
    /// `doctor`'s checks.
    Checks(Vec<Check>),
    /// The request is over; `Err` says why it failed: the plugin's error,
    /// else how its process ended.
    Done(Result<(), String>),
}

/// What dffy asks a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    List,
    Detect,
    Prepare {
        id: String,
    },
    Pull {
        id: String,
    },
    /// Plans the review's drafts, then those of `drafts`: the local
    /// review's file and the first comments of its threads in view, the
    /// ones a post would take along.
    DryRun {
        id: String,
        drafts: Option<(PathBuf, Vec<String>)>,
    },
    Post {
        id: String,
        event: String,
        /// The labels voted on in the dialog, by name; one left alone isn't
        /// sent, so a vote given before stays.
        labels: Vec<(String, i32)>,
        summary: Option<String>,
    },
    Doctor,
}

/// What a review's request carried that its answer doesn't repeat: your
/// comments pending on the host, and the resolves and reopens sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Sent {
    pub pending: usize,
    pub resolves: usize,
}

impl Request {
    fn method(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Detect => "detect",
            Self::Prepare { .. } => "prepare",
            Self::Pull { .. } => "pull",
            Self::DryRun { .. } => "dryRun",
            Self::Post { .. } => "post",
            Self::Doctor => "doctor",
        }
    }

    /// Whether a later one replaces it: one of these runs at a time.
    pub(crate) fn exclusive(&self) -> bool {
        matches!(
            self,
            Self::Prepare { .. } | Self::Pull { .. } | Self::Post { .. }
        )
    }

    /// The review it's of, for those of one.
    fn review(&self) -> Option<&str> {
        match self {
            Self::Prepare { id }
            | Self::Pull { id }
            | Self::DryRun { id, .. }
            | Self::Post { id, .. } => Some(id),
            Self::List | Self::Detect | Self::Doctor => None,
        }
    }

    /// Its params, from the review's comments `file` as it is now, and
    /// what they carried that the answer won't repeat. A review's request
    /// carries only what the plugin needs of the file, never the file:
    /// `prepare` and `pull` the comments the host has ([`known`]); `dryRun`
    /// and `post` your drafts ([`drafts`]), a dry run's with the local
    /// drafts it names after them, and the resolves not sent ([`resolve`]).
    fn params(&self, file: Option<&Path>) -> Result<(String, Sent), String> {
        let text = |s: &str| Value::String(s.to_owned());
        let mut out = Vec::new();
        let mut sent = Sent::default();
        if let Some(id) = self.review() {
            out.push(("id".to_owned(), text(id)));
        }
        match self {
            Self::List | Self::Detect | Self::Doctor => {}
            Self::Prepare { .. } | Self::Pull { .. } => {
                let review = file.map(crate::review::read).transpose()?;
                let known = known(&review.unwrap_or_default());
                out.push(("known".to_owned(), Value::Array(known)));
            }
            Self::DryRun { .. } | Self::Post { .. } => {
                if let Self::Post {
                    event,
                    labels,
                    summary,
                    ..
                } = self
                {
                    out.push(("event".to_owned(), text(event)));
                    if !labels.is_empty() {
                        let votes = labels
                            .iter()
                            .map(|(name, value)| (name.clone(), Value::Number(f64::from(*value))));
                        out.push(("labels".to_owned(), Value::Object(votes.collect())));
                    }
                    if let Some(summary) = summary {
                        out.push(("summary".to_owned(), text(summary)));
                    }
                }
                let review = file.map(crate::review::read).transpose()?;
                let review = review.unwrap_or_default();
                let mut sending = drafts(&review);
                if let Self::DryRun {
                    drafts: Some((local, roots)),
                    ..
                } = self
                {
                    let mut local = crate::review::read(local)?;
                    (local.comments)
                        .retain(|c| roots.contains(c.in_reply_to.as_ref().unwrap_or(&c.id)));
                    sending.extend(drafts(&local));
                }
                let resolves: Vec<Value> = (review.comments.iter())
                    .filter(|c| c.resolve_unsent())
                    .map(resolve)
                    .collect();
                sent = Sent {
                    pending: review.comments.iter().filter(|c| c.pending).count(),
                    resolves: resolves.len(),
                };
                out.push(("drafts".to_owned(), Value::Array(sending)));
                out.push(("resolves".to_owned(), Value::Array(resolves)));
            }
        }
        let mut params = String::new();
        json::write(&mut params, &Value::Object(out));
        Ok((params, sent))
    }
}

/// What `prepare` and `pull` carry of each comment the host has, by its
/// `ref`, for the plugin to merge what it imports: whether it has its
/// lines already, so a thread whose lines can't be read again keeps them;
/// whether it's pending; and the integration's own fields as the file
/// holds them. A resolve or reopen not sent isn't among them: dffy keeps
/// it through what the import says (see [`crate::review::Op::Upsert`]).
fn known(review: &crate::review::Review) -> Vec<Value> {
    let flag = |key: &str, on: bool| on.then(|| (key.to_owned(), Value::Bool(true)));
    (review.comments.iter())
        .filter_map(|c| {
            let reference = Value::String(c.reference.clone()?);
            let mut entry = vec![("ref".to_owned(), reference)];
            entry.extend(flag("anchored", c.anchor.is_some()));
            entry.extend(flag("pending", c.pending));
            entry.extend(c.extra.iter().cloned());
            Some(Value::Object(entry))
        })
        .collect()
}

/// Your drafts in `review`, in its order, each with what a host needs to
/// post it: where it is, its anchor and `distance`, its words, whether a
/// thread it starts is resolved, and for a reply, its thread's first
/// comment: by `ref`, with the integration's fields, when that's posted,
/// else by `id`, a draft sent before it.
fn drafts(review: &crate::review::Review) -> Vec<Value> {
    const SENT: [&str; 11] = [
        "id",
        "path",
        "old_path",
        "side",
        "start_side",
        "start",
        "end",
        "text",
        "suggestion",
        "anchor",
        "distance",
    ];
    let reply_to = |root: &str| {
        let root = review.comments.iter().find(|c| c.id == root);
        let mut to = match root.and_then(|r| r.reference.clone()) {
            Some(reference) => vec![("ref".to_owned(), Value::String(reference))],
            None => vec![(
                "id".to_owned(),
                Value::String(root.map_or("", |r| &r.id).to_owned()),
            )],
        };
        to.extend(
            root.filter(|r| r.reference.is_some())
                .into_iter()
                .flat_map(|r| r.extra.iter().cloned()),
        );
        Value::Object(to)
    };
    review
        .drafts()
        .map(|c| {
            let fields = c
                .to_fields()
                .into_iter()
                .filter_map(|(key, value)| match key.as_str() {
                    "in_reply_to" => Some((key, reply_to(c.in_reply_to.as_deref()?))),
                    "resolved" => (c.in_reply_to.is_none() && c.resolved).then_some((key, value)),
                    k if SENT.contains(&k) => Some((key, value)),
                    _ => None,
                });
            Value::Object(fields.collect())
        })
        .collect()
}

/// A thread resolved or reopened here and not sent: its first comment's
/// `ref`, the state to send, where it is, for words about it, and the
/// integration's fields.
fn resolve(root: &crate::review::Comment) -> Value {
    let mut out = vec![
        (
            "ref".to_owned(),
            Value::String(root.reference.clone().unwrap_or_default()),
        ),
        ("resolved".to_owned(), Value::Bool(root.resolved)),
    ];
    out.extend(
        (root.to_fields().into_iter())
            .filter(|(key, _)| matches!(key.as_str(), "path" | "side" | "end")),
    );
    out.extend(root.extra.iter().cloned());
    Value::Object(out)
}

/// What a plugin says on its own, about the review it was told dffy shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Push {
    /// The review's head on the host, which may not be the session's.
    HeadMoved { id: String, head: String },
    /// The review changed on the host since its comments were last
    /// imported: new comments, most likely.
    CommentsChanged { id: String },
    /// Something to say in the status bar, such as why watching failed.
    Log(String),
}

fn count(value: Option<&Value>) -> usize {
    value.and_then(Value::as_usize).unwrap_or(0)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A full commit id, SHA-1's or SHA-256's, which dffy puts in revisions:
/// hex only, so a plugin can't name anything else there, and whole, so it
/// can't turn ambiguous as the repository grows.
fn commit(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned)
}

/// A review's `id` and optional `label`. Both show, so they're one
/// printable line; the id can't pass for a flag, as some plugins take it
/// on a command line.
fn review_id(value: &Value) -> Result<(String, Option<String>), String> {
    let printable = |s: &str| !s.is_empty() && s.len() <= 200 && !s.contains(char::is_control);
    let id = text(value, "id")
        .filter(|id| printable(id) && !id.starts_with('-'))
        .ok_or("no review id, or one that isn't printable text")?;
    let label = match value.get("label") {
        None | Some(Value::Null) => None,
        Some(label) => Some(
            label
                .as_str()
                .filter(|l| printable(l))
                .ok_or("a review label that isn't printable text")?
                .to_owned(),
        ),
    };
    Ok((id, label))
}

/// A session, its comments file the review's in `reviews`, the plugin's
/// directory of them.
fn session(value: &Value, reviews: Option<&Path>) -> Result<Session, String> {
    let reviews = reviews.ok_or("there's no repository to keep the review's comments in")?;
    let side = |key: &str| {
        value
            .get(key)
            .and_then(commit)
            .ok_or_else(|| format!("the session's {key} isn't a commit"))
    };
    let (id, label) = review_id(value)?;
    let session = Session {
        comments: crate::review::review_file(reviews, &id),
        id,
        label,
        title: text(value, "title"),
        url: text(value, "url"),
        base: side("base")?,
        head: side("head")?,
        last_reviewed_head: match value.get("last_reviewed_head") {
            None | Some(Value::Null) => None,
            Some(_) => Some(side("last_reviewed_head")?),
        },
        state: match value.get("state").and_then(Value::as_str) {
            Some("open") => Some(State::Open),
            Some("closed") => Some(State::Closed),
            Some("merged") => Some(State::Merged),
            _ => None,
        },
        labels: match value.get("labels") {
            None | Some(Value::Null) => Vec::new(),
            Some(labels) => review_labels(labels)?,
        },
        // Only ever shown, never run; a line of text or nothing.
        checkout: text(value, "checkout")
            .filter(|c| !c.is_empty() && c.len() <= 200 && !c.contains(char::is_control)),
    };
    Ok(session)
}

/// Your drafts in review `id`'s comments file, in `reviews`.
fn drafts_in(reviews: &Path, id: &str) -> usize {
    let file = crate::review::review_file(reviews, id);
    crate::review::read(&file).map_or(0, |review| review.drafts().count())
}

fn listed(value: &Value) -> Result<Listed, String> {
    let (id, label) = review_id(value)?;
    Ok(Listed {
        id,
        label,
        title: text(value, "title").unwrap_or_default(),
        author: text(value, "author").unwrap_or_default(),
        reason: match value.get("reason").and_then(Value::as_str) {
            Some("current_branch") => Reason::CurrentBranch,
            Some("review_requested") => Reason::ReviewRequested,
            Some("mine") => Reason::Mine,
            _ => Reason::Other,
        },
        // Words dffy draws as they are, so one printable line or none.
        group: text(value, "group").and_then(|g| noun("group", g, 40).ok()),
        drafts: 0,
    })
}

fn draft(value: &Value) -> Result<Draft, String> {
    let id = text(value, "id").ok_or("a draft without an id")?;
    let plan = match value.get("plan").and_then(Value::as_str) {
        Some("skip") => Plan::Skip(text(value, "reason").unwrap_or_default()),
        Some("file") => Plan::File,
        _ => Plan::Line,
    };
    Ok(Draft {
        id,
        path: text(value, "path").unwrap_or_default(),
        plan,
    })
}

fn plans(value: &Value, key: &str) -> Result<Vec<Said>, String> {
    let Some(list) = value.get(key) else {
        return Ok(Vec::new());
    };
    let list = list
        .as_array()
        .ok_or_else(|| format!("{key} isn't a list"))?;
    list.iter().map(|d| draft(d).map(Said::Draft)).collect()
}

/// What `method`'s result, or a failed one's `data`, says, with what dffy
/// keeps of each review in `reviews`, the plugin's directory of them, and
/// what the request carried, which a dry run's counts take.
fn answer(
    method: &str,
    result: &Value,
    (reviews, sent): (Option<&Path>, Sent),
) -> Result<Vec<Said>, String> {
    Ok(match method {
        "list" => {
            let entries = result.get("reviews").and_then(Value::as_array);
            let entries = entries.ok_or("its list has no reviews")?;
            let mut listed: Vec<Listed> = entries.iter().map(listed).collect::<Result<_, _>>()?;
            if let Some(reviews) = reviews {
                for review in &mut listed {
                    review.drafts = drafts_in(reviews, &review.id);
                }
            }
            vec![Said::Listed(listed)]
        }
        "detect" => vec![Said::Detected(match result.get("review") {
            None | Some(Value::Null) => None,
            Some(review) => Some(session(review, reviews)?),
        })],
        "dryRun" => {
            let mut said = plans(result, "drafts")?;
            let (mut line, mut file, mut skip) = (0, 0, 0);
            for draft in &said {
                if let Said::Draft(draft) = draft {
                    *match draft.plan {
                        Plan::Line => &mut line,
                        Plan::File => &mut file,
                        Plan::Skip(_) => &mut skip,
                    } += 1;
                }
            }
            said.push(Said::Planned {
                line,
                file,
                skip,
                pending: sent.pending,
                resolves: sent.resolves,
            });
            said
        }
        "post" if result.get("posted").is_some() => {
            let mut said = plans(result, "unposted")?;
            said.push(Said::Posted {
                posted: count(result.get("posted")),
                failed: count(result.get("failed")),
                pending: count(result.get("pending")),
                url: text(result, "url"),
                last_reviewed_head: result.get("last_reviewed_head").and_then(commit),
                labels: match result.get("labels") {
                    None | Some(Value::Null) => None,
                    Some(labels) => Some(review_labels(labels)?),
                },
            });
            said
        }
        "doctor" => vec![Said::Checks(checks(result)?)],
        _ => Vec::new(),
    })
}

/// The checks of a `doctor` result, a review plugin's or the agent
/// plugin's.
pub(crate) fn checks(result: &Value) -> Result<Vec<Check>, String> {
    let checks = result.get("checks").and_then(Value::as_array);
    let checks = checks.ok_or("its doctor has no checks")?;
    checks
        .iter()
        .map(|check| {
            Ok(Check {
                name: text(check, "name").ok_or("a check without a name")?,
                ok: check.get("ok") == Some(&Value::Bool(true)),
                detail: text(check, "detail").unwrap_or_default(),
                brief: text(check, "brief"),
                info: check.get("info") == Some(&Value::Bool(true)),
            })
        })
        .collect()
}

type Deliver = Box<dyn FnMut(Said) -> bool + Send>;

/// A running review plugin: one process, restarted when it crashes, that
/// answers requests and pushes what changes on the host. Requests made
/// before it has initialized wait for it. Cloning shares the process.
#[derive(Clone)]
pub(crate) struct Plugin(rpc::Client<Init, PluginState, Ask>);

/// A request made, to cancel: it still ends with its `Done`, which says so.
pub(crate) type Run = rpc::Run<Init, PluginState, Ask>;

type Shared = rpc::Shared<Init, PluginState, Ask>;
type Inner = rpc::Inner<Init, PluginState, Ask>;
type Guard<'a> = rpc::Guard<'a, Init, PluginState, Ask>;

const KIND: rpc::Kind<Init, PluginState, Ask> = rpc::Kind {
    thread: "dffy-review",
    protocol: PROTOCOL,
    init_params: |_| String::new(),
    // What it starts is in its group.
    grace: Duration::ZERO,
    initialized,
    answered: response,
    notified,
    failed: |mut pending, why| {
        pending.deliver(Said::Done(Err(why)));
    },
};

/// What dffy keeps of a review plugin, under its client's lock.
pub(crate) struct PluginState {
    /// Where its reviews' comments files are, in a repository.
    reviews: Option<PathBuf>,
    /// The review dffy shows, for a plugin that watches.
    watch: Option<String>,
    idle: bool,
    on_push: Option<Box<dyn FnMut(Push) -> bool + Send>>,
    /// Pushes that came before anything took them, as a `prepare` started
    /// before the viewer may bring.
    pushed: Vec<Push>,
}

/// Whether the plugin is initialized and watches, so it's told what dffy
/// shows and whether you're idle as that changes.
fn watching(inner: &Inner) -> bool {
    inner.ready() && inner.init.as_ref().is_some_and(|i| i.watch)
}

/// A review request, waiting for its answer.
pub(crate) struct Ask {
    method: &'static str,
    /// The comments file its `comments/apply` go to: the review's.
    comments: Option<PathBuf>,
    /// What it carried that its answer doesn't repeat, once it's sent.
    sent: Sent,
    /// A post's event, which may submit the review.
    event: Option<String>,
    /// Why operations it sent couldn't be written: it ends saying so.
    unwritten: Option<String>,
    /// A post's lock on its review (see [`crate::review::lock_post`]),
    /// let go of as it ends, however it does.
    posting: Option<std::fs::File>,
    deliver: Deliver,
}

impl Pending<Ask> {
    /// Hands `said` on, unless the request ended here; whether it's still
    /// wanted. An ended request's operations are still written until the
    /// plugin answers, so a post's refs aren't lost. A post lets go of its
    /// lock before its `Done` is handed on, so whoever hears of the end can
    /// post again at once.
    fn deliver(&mut self, said: Said) -> bool {
        if matches!(said, Said::Done(_)) {
            self.request.posting = None;
        }
        !self.ended && (self.request.deliver)(said)
    }
}

impl Plugin {
    /// Starts `program` in `root`, or where dffy runs, detached from the
    /// terminal. Its reviews' comments files are in `reviews`, which dffy
    /// writes what it says to; its `initialize` answer goes in `cache`.
    pub(crate) fn start(
        program: &Program,
        (root, reviews): (Option<&Path>, Option<PathBuf>),
        cache: Option<Cache>,
    ) -> io::Result<Self> {
        Self::spawn(
            format!("{}{}", super::REVIEW_PREFIX, program.name),
            rpc::detached(program, root),
            (
                reviews,
                cache.map(|c| (c, rpc::cache_key(PROTOCOL, program))),
            ),
        )
    }

    fn spawn(
        name: String,
        command: rpc::MakeCommand,
        (reviews, cache): (Option<PathBuf>, Option<(Cache, Vec<u8>)>),
    ) -> io::Result<Self> {
        let state = PluginState {
            reviews,
            watch: None,
            idle: false,
            on_push: None,
            pushed: Vec::new(),
        };
        rpc::Client::spawn(name, command, cache, KIND, state).map(Self)
    }

    /// Hands the naming to `ready` once the plugin has initialized, or why
    /// it can't: now, if it has.
    pub(crate) fn on_ready(&self, ready: impl FnOnce(Result<Naming, String>) + Send + 'static) {
        (self.0).on_ready(move |init| ready(init.map(|i| i.naming.clone())));
    }

    /// Hands what the plugin pushes to `push`, what it pushed before first,
    /// until it returns false.
    pub(crate) fn on_push(&self, mut push: impl FnMut(Push) -> bool + Send + 'static) {
        let mut inner = self.0.lock();
        for pushed in std::mem::take(&mut inner.kind.pushed) {
            if !push(pushed) {
                return;
            }
        }
        inner.kind.on_push = Some(Box::new(push));
    }

    /// Sends `request`, and each thing it says to `deliver` from a thread of
    /// the plugin's, then [`Said::Done`]; `deliver` mustn't call back into
    /// the plugin. Once `deliver` returns false, nothing more goes to it.
    /// What the plugin writes of the review is in its comments file before
    /// what follows it is delivered.
    pub(crate) fn request(
        &self,
        request: &Request,
        deliver: impl FnMut(Said) -> bool + Send + 'static,
    ) -> Run {
        let comments = {
            let inner = self.0.lock();
            (request.review())
                .zip(inner.kind.reviews.as_deref())
                .map(|(review, dir)| crate::review::review_file(dir, review))
        };
        let (asked, file) = (request.clone(), comments.clone());
        let params = move |shared: &Shared, id: u64| {
            // Taken before the drafts are read, and held until the post ends:
            // two viewers posting one review would each post its drafts.
            let posting = match (&asked, file.as_deref()) {
                (Request::Post { .. }, Some(file)) => Some(crate::review::lock_post(file)),
                _ => None,
            };
            let params = posting.transpose().and_then(|posting| {
                (asked.params(file.as_deref()))
                    .map(|params| (params, posting))
                    .map_err(|e| format!("the comments: {e}"))
            });
            let mut inner = shared.lock();
            match params {
                Ok(((params, sent), posting)) => {
                    if let Some(pending) = inner.pending.get_mut(&id) {
                        pending.request.sent = sent;
                        pending.request.posting = posting;
                    }
                    Some(params)
                }
                Err(e) => {
                    if let Some(mut pending) = inner.pending.remove(&id) {
                        (pending.request.deliver)(Said::Done(Err(e)));
                    }
                    None
                }
            }
        };
        let ask = Ask {
            method: request.method(),
            comments,
            sent: Sent::default(),
            event: match request {
                Request::Post { event, .. } => Some(event.clone()),
                _ => None,
            },
            unwritten: None,
            posting: None,
            deliver: Box::new(deliver),
        };
        self.0.request(request.method(), ask, params)
    }

    /// Tells a plugin that watches which review dffy shows now, if any.
    pub(crate) fn watch(&self, review: Option<&str>) {
        let mut inner = self.0.lock();
        if inner.kind.watch.as_deref() == review {
            return;
        }
        inner.kind.watch = review.map(str::to_owned);
        if watching(&inner) {
            inner.notify("watch", &watch_params(inner.kind.watch.as_deref()));
        }
    }

    /// Tells a plugin that watches whether you're away, when that changes.
    pub(crate) fn idle(&self, idle: bool) {
        let mut inner = self.0.lock();
        if inner.kind.idle == idle {
            return;
        }
        inner.kind.idle = idle;
        if watching(&inner) {
            inner.notify("idle/changed", &idle_params(idle));
        }
    }

    /// Asks the plugin to shut down and exit, then kills it with whatever
    /// it started: nothing it does from here on is waited for.
    pub(crate) fn close(&self) {
        self.0.close();
    }
}

fn watch_params(review: Option<&str>) -> String {
    let mut params = String::from("{\"id\":");
    match review {
        Some(id) => json::string(&mut params, id),
        None => params.push_str("null"),
    }
    params.push('}');
    params
}

fn idle_params(idle: bool) -> String {
    format!("{{\"idle\":{idle}}}")
}

/// The answer to `initialize`, for a process that's ready.
fn initialized(inner: &mut Inner, result: &Value) -> Result<Init, String> {
    let init = Init::decode(result)?;
    // What dffy shows before what it asks, so the requests are answered
    // knowing it.
    if init.watch {
        if inner.kind.watch.is_some() {
            inner.notify("watch", &watch_params(inner.kind.watch.as_deref()));
        }
        if inner.kind.idle {
            inner.notify("idle/changed", &idle_params(true));
        }
    }
    Ok(init)
}

/// A response to `pending`'s request: what it says, then its `Done`.
fn response(shared: &Shared, inner: Guard<'_>, mut pending: Pending<Ask>, value: &Value) {
    let submits = pending.request.event.as_ref().is_some_and(|event| {
        (inner.init.iter())
            .flat_map(|i| &i.naming.events)
            .any(|e| e.id == *event && e.submits)
    });
    let reviews = inner.kind.reviews.clone();
    // What the answer reads, it reads without holding up requests.
    drop(inner);
    let said = answered(
        (shared.name(), reviews.as_deref()),
        &mut pending.request,
        value,
        submits,
    );
    for said in said {
        if !pending.deliver(said) {
            break;
        }
    }
}

/// What a response to `ask` says, then its `Done`: failed when what it
/// sent couldn't be written. A post whose event `submits` that ends well
/// leaves nothing in the review for you only.
fn answered(
    (name, reviews): (&str, Option<&Path>),
    ask: &mut Ask,
    value: &Value,
    submits: bool,
) -> Vec<Said> {
    let mut said = read_answer((name, reviews), ask, value);
    if let (Some(why), Some(done)) = (ask.unwritten.take(), said.last_mut()) {
        *done = Said::Done(Err(why));
    }
    if let (true, Some(file), Some(done @ Said::Done(Ok(())))) =
        (submits, &ask.comments, said.last_mut())
        && let Err(e) = crate::review::submitted(file)
    {
        *done = Said::Done(Err(format!(
            "the review went, but its comments still say pending: {e}"
        )));
    }
    said
}

/// What a response to `ask` says, as the plugin answered; `name` is the
/// plugin's, and `reviews` its directory of comments files.
fn read_answer((name, reviews): (&str, Option<&Path>), ask: &Ask, value: &Value) -> Vec<Said> {
    let known = (reviews, ask.sent);
    let method = ask.method;
    match (value.get("result"), value.get("error")) {
        (_, Some(error)) if *error != Value::Null => {
            let mut said = error
                .get("data")
                .and_then(|data| answer(method, data, known).ok())
                .unwrap_or_default();
            said.push(Said::Done(Err(rpc::error_message(name, method, error))));
            said
        }
        (Some(result), _) => match answer(method, result, known) {
            Ok(mut said) => {
                said.push(Said::Done(Ok(())));
                said
            }
            Err(e) => vec![Said::Done(Err(format!("{name} {method}: {e}")))],
        },
        (None, _) => vec![Said::Done(Err(format!(
            "{name} {method}: an answer without a result"
        )))],
    }
}

/// Applies `ops`, which request `id` says to write, to its review's
/// comments `file`. When they can't be, the plugin is asked to stop the
/// request, as what it does next could go unrecorded, and the request ends
/// saying why; what it sends until it stops is still written, as a post's
/// refs must be.
fn write_comments(shared: &Shared, (id, file): (u64, &Path), ops: Option<&Value>) {
    let mut text = String::new();
    json::write(&mut text, ops.unwrap_or(&Value::Null));
    let Err(e) = crate::review::apply_json(file, text.as_bytes(), None) else {
        return;
    };
    let mut inner = shared.lock();
    let Some(pending) = (inner.pending.get_mut(&id)).filter(|p| p.request.unwritten.is_none())
    else {
        return;
    };
    pending.request.unwritten = Some(format!("{}'s comments not written: {e}", shared.name()));
    if !pending.ended {
        inner.send_cancel(id);
    }
}

/// A notification from the plugin: about a request, or pushed.
fn notified(shared: &Shared, mut inner: Guard<'_>, method: &str, params: &Value) {
    let request = params.get("request").and_then(rpc::request_id);
    if let (Some(id), "comments/apply") = (request, method) {
        let Some(file) = (inner.pending.get(&id)).and_then(|p| p.request.comments.clone()) else {
            return;
        };
        drop(inner);
        write_comments(shared, (id, &file), params.get("ops"));
        return;
    }
    let said = match method {
        "progress" => text(params, "progress").map(Said::Progress),
        "session" => {
            let Some(session) = params.get("session") else {
                return;
            };
            Some(
                match self::session(session, inner.kind.reviews.as_deref()) {
                    Ok(session) => Said::Session(session),
                    Err(e) => Said::Done(Err(format!("{}: {e}", shared.name()))),
                },
            )
        }
        _ => None,
    };
    if let (Some(said), Some(id)) = (said, request) {
        let done = matches!(said, Said::Done(_));
        if let Some(pending) = inner.pending.get_mut(&id)
            && (!pending.deliver(said) || done)
        {
            inner.end(id);
        }
        return;
    }
    let review = || text(params, "id");
    let push = match method {
        "head/moved" => review()
            .zip(params.get("head").and_then(commit))
            .map(|(id, head)| Push::HeadMoved { id, head }),
        "comments/changed" => review().map(|id| Push::CommentsChanged { id }),
        "log" => text(params, "message").map(Push::Log),
        _ => None,
    };
    match (push, &mut inner.kind.on_push) {
        (Some(push), Some(on_push)) => {
            if !on_push(push) {
                inner.kind.on_push = None;
            }
        }
        (Some(push), None) => inner.kind.pushed.push(push),
        (None, _) => {}
    }
}

#[cfg(test)]
mod tests;
