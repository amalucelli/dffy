//! Review plugins (experimental): programs named `dffy-review-<name>` that
//! keep the comments file in step with a code host, since dffy never
//! touches the network. dffy runs one of a plugin's verbs in the
//! background and reads what it says, one JSON object a line, carrying
//! [`PROTOCOL`]; docs/plugins.md has the verbs and lines.
//!
//! A run has no terminal: it starts in a session of its own (through
//! [`exec_detached`]), so a passphrase or password prompt fails instead of
//! drawing over the viewer. Like a renderer, it runs in its own process
//! group, killed with everything it started when dffy exits.

use std::ffi::OsString;
use std::io::{self, BufRead as _, BufReader, Read};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use dffy_core::cache::{self, Cache};
use rustix::process::{Pid, WaitId, WaitIdOptions};

use super::json::{self, Value};
use super::{Program, kill_group, running};

/// The protocol version, in `DFFY_REVIEW_PROTOCOL` and in every line a
/// plugin prints; a line of another version stops the run.
pub(crate) const PROTOCOL: usize = 1;

/// The first argument that makes `dffy` run a review plugin in its place,
/// in a session of its own (see [`exec_detached`]).
pub(crate) const DETACH: &str = "__review-plugin";

/// `dffy __review-plugin <program> <args>…`: leaves the terminal's
/// session, then becomes `program`. std starts a process in a new session
/// only from `pre_exec`, which needs unsafe code; this costs an exec.
pub(crate) fn exec_detached(mut args: impl Iterator<Item = OsString>) -> ExitCode {
    let _ = rustix::process::setsid();
    let Some(program) = args.next() else {
        return ExitCode::from(2);
    };
    let e = Command::new(&program).args(args).exec();
    eprintln!("can't run {}: {e}", Path::new(&program).display());
    ExitCode::from(127)
}

/// How long `describe` may take. It runs before dffy parses its arguments,
/// until its answer is cached, so a hung plugin can't hold every start.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the plugin's `doctor` may take: its checks may ask the host.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// What a plugin's `describe` says: the subcommand dffy adds for it, the
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

/// One way `post` can end a review, as the plugin names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewEvent {
    /// Passed as `--event <id>`.
    pub id: String,
    /// Picks it in the dialog.
    pub key: char,
    pub label: String,
    /// The status bar's words once it went: `approved`.
    pub done: String,
    /// Sending it ends the review on the host, so drafts the host refused
    /// hold it back (see [`Said::Posted`]); a `false` event leaves the
    /// review open there.
    pub submits: bool,
}

/// The dialog's own keys, which an event's key can't be.
const DIALOG_KEYS: [char; 4] = ['j', 'k', 's', 'v'];

impl Naming {
    /// Reads a `describe` answer. A command is lowercase letters, digits
    /// and `-`, starting with a letter. Words are one line of printable
    /// text, since help and the viewer print them as they are.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, String> {
        let value = answer(bytes)?;
        let field =
            |key: &str| text(&value, key).ok_or_else(|| format!("its describe has no {key}"));
        let command = field("command")?;
        if !command.starts_with(|c: char| c.is_ascii_lowercase())
            || !command
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(format!(
                "its command '{}' isn't lowercase letters, digits and -, starting with a letter",
                command.escape_debug()
            ));
        }
        let noun = noun("noun", field("noun")?, 40)?;
        let short = match text(&value, "short") {
            Some(short) => self::noun("short", short, 40)?,
            None => noun.clone(),
        };
        let id_prefix = text(&value, "id_prefix").unwrap_or_default();
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
        let outdated_reply_warning = text(&value, "outdated_reply_warning")
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

/// The `events` of a describe answer: 1 to 9, each id a word and each key
/// one letter or digit the dialog doesn't take, neither repeated.
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
                    "its event {id}'s key '{}' isn't one letter or digit other than j, k, s and v",
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

#[derive(Debug, Clone)]
pub(crate) struct Described {
    pub program: Program,
    pub naming: Naming,
}

/// `program`'s naming. Its `describe` answer is kept in the disk cache
/// while the program is the same file (path, modification time and size),
/// so dffy doesn't run the plugin on every start; only an answer that
/// reads as one is kept. A command `reserved` says is dffy's own is
/// refused, cached or not. A failure isn't kept: the next start asks again.
pub(crate) fn describe(
    program: &Program,
    cache: Option<&Cache>,
    reserved: impl Fn(&str) -> bool,
) -> Result<Naming, String> {
    let mut key = Vec::with_capacity(64);
    key.extend_from_slice(&PROTOCOL.to_le_bytes());
    key.extend_from_slice(program.path.as_os_str().as_encoded_bytes());
    key.push(0);
    key.extend_from_slice(&program.stamp.0.to_le_bytes());
    key.extend_from_slice(&program.stamp.1.to_le_bytes());
    let cached = cache.and_then(|c| c.get(cache::Kind::Described, &key));
    let naming = if let Some(Ok(naming)) = cached.map(|answer| Naming::decode(&answer)) {
        naming
    } else {
        let answer = ask(program, "describe", DESCRIBE_TIMEOUT)?;
        let naming = Naming::decode(&answer)?;
        if let Some(cache) = cache {
            cache.put(cache::Kind::Described, &key, &answer);
        }
        naming
    };
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
}

/// The plugin's checks of what it needs, for `dffy doctor`; never cached.
pub(crate) fn doctor(program: &Program) -> Result<Vec<Check>, String> {
    let value = answer(&ask(program, "doctor", DOCTOR_TIMEOUT)?)?;
    value
        .get("checks")
        .and_then(Value::as_array)
        .ok_or("its doctor has no checks")?
        .iter()
        .map(|check| {
            Ok(Check {
                name: text(check, "name").ok_or("a check without a name")?,
                ok: check.get("ok") == Some(&Value::Bool(true)),
                detail: text(check, "detail").unwrap_or_default(),
            })
        })
        .collect()
}

/// Runs `verb`, which answers with one line and needs no repository, the
/// way the other verbs run: detached from the terminal, with stdin closed.
/// Returns what it printed.
fn ask(program: &Program, verb: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(&exe);
    command
        .arg(DETACH)
        .arg(&program.path)
        .arg(verb)
        .env("DFFY", exe)
        .env("DFFY_REVIEW_PROTOCOL", PROTOCOL.to_string())
        .env_remove("DFFY_COMMENTS");
    let (out, status) =
        super::run(command, Vec::new(), timeout, || false).map_err(|failure| match failure {
            super::Failure::Timeout => format!("its {verb} took over {}s", timeout.as_secs()),
            failure => format!("its {verb} {failure}"),
        })?;
    let first = out
        .split(|&b| b == b'\n')
        .find(|l| !l.trim_ascii().is_empty());
    // Its own message says more than how it exited.
    if let Some(Ok(Some(Line::Error(message)))) = first.map(decode) {
        return Err(message);
    }
    match status.code() {
        Some(0) => answer(&out)
            .map(|_| out)
            .map_err(|e| format!("its {verb}: {e}")),
        Some(2) => Err(format!(
            "it doesn't know {verb}; it may be older than this dffy"
        )),
        _ => Err(format!("its {verb} {}", super::exited(status))),
    }
}

/// The first line of a one-line answer, checked: an object of this
/// protocol's version, without an error.
fn answer(bytes: &[u8]) -> Result<Value, String> {
    let line = bytes
        .split(|&b| b == b'\n')
        .find(|l| !l.trim_ascii().is_empty())
        .ok_or("it printed nothing")?;
    match decode(line)? {
        Some(Line::Error(message)) => Err(message),
        _ => json::parse(line),
    }
}

/// A review, from a session line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    /// The plugin's id, which its verbs take.
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
    /// The comments file, absolute.
    pub comments: PathBuf,
}

impl Session {
    pub(crate) fn shown(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
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

/// A draft line: what `post --dry-run` plans for a draft, or a draft a
/// `post` left unposted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Draft {
    pub id: String,
    pub path: String,
    pub plan: Plan,
}

/// What a run says, in order; [`Said::Done`] is last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Said {
    Session(Session),
    /// `detect`: the branch checked out's review, as a session, if it has
    /// one.
    Detected(Option<Session>),
    Progress(String),
    Listed(Vec<Listed>),
    /// `check`: the review's head on the host, and whether it isn't the
    /// session's.
    Head {
        head: String,
        moved: bool,
    },
    Draft(Draft),
    /// `post --dry-run` finished: how many drafts go on a line, on the
    /// file, and nowhere.
    Planned {
        line: usize,
        file: usize,
        skip: usize,
    },
    /// `post` finished: comments posted and not, whether the event wasn't
    /// applied, with the plugin's words for where that leaves the review,
    /// and the review's link.
    Posted {
        posted: usize,
        failed: usize,
        held: bool,
        held_message: Option<String>,
        url: Option<String>,
    },
    /// The run is over; `Err` says why it failed: the plugin's own error
    /// line, else how it exited.
    Done(Result<(), String>),
}

#[derive(Debug, PartialEq, Eq)]
enum Line {
    Said(Said),
    Error(String),
}

/// Reads one line; `None` for a line of no kind dffy knows, which a later
/// plugin may print.
fn decode(bytes: &[u8]) -> Result<Option<Line>, String> {
    let value = json::parse(bytes)?;
    if !matches!(value, Value::Object(_)) {
        return Err("a line isn't a JSON object".to_owned());
    }
    match value.get("version").map(Value::as_usize) {
        None | Some(Some(PROTOCOL)) => {}
        Some(Some(other)) => {
            return Err(format!(
                "it speaks review protocol {other}, and dffy speaks {PROTOCOL}"
            ));
        }
        Some(None) => return Err("a version that isn't a number".to_owned()),
    }
    if let Some(error) = value.get("error").filter(|e| **e != Value::Null) {
        let message = error.as_str().unwrap_or("an error that isn't text");
        return Ok(Some(Line::Error(message.to_owned())));
    }
    let said = if let Some(s) = value.get("session") {
        Said::Session(session(s)?)
    } else if let Some(review) = value.get("review") {
        Said::Detected(match review {
            Value::Null => None,
            review => Some(session(review)?),
        })
    } else if let Some(p) = value.get("progress") {
        Said::Progress(p.as_str().ok_or("progress isn't text")?.to_owned())
    } else if let Some(reviews) = value.get("reviews") {
        let reviews = reviews.as_array().ok_or("reviews isn't a list")?;
        Said::Listed(reviews.iter().map(listed).collect::<Result<_, _>>()?)
    } else if let Some(head) = value.get("head") {
        Said::Head {
            head: commit(head).ok_or("head isn't a commit")?,
            moved: value.get("moved") == Some(&Value::Bool(true)),
        }
    } else if let Some(draft) = value.get("draft") {
        let id = text(draft, "id").ok_or("a draft line without an id")?;
        let plan = match draft.get("plan").and_then(Value::as_str) {
            Some("skip") => Plan::Skip(text(draft, "reason").unwrap_or_default()),
            Some("file") => Plan::File,
            _ => Plan::Line,
        };
        Said::Draft(Draft {
            id,
            path: text(draft, "path").unwrap_or_default(),
            plan,
        })
    } else if let Some(planned) = value.get("planned") {
        Said::Planned {
            line: count(planned.get("line")),
            file: count(planned.get("file")),
            skip: count(planned.get("skip")),
        }
    } else if let Some(posted) = value.get("posted") {
        Said::Posted {
            posted: count(Some(posted)),
            failed: count(value.get("failed")),
            held: value.get("held") == Some(&Value::Bool(true)),
            held_message: text(&value, "held_message")
                .and_then(|m| noun("held_message", m, 200).ok()),
            url: text(&value, "url"),
        }
    } else {
        return Ok(None);
    };
    Ok(Some(Line::Said(said)))
}

fn count(value: Option<&Value>) -> usize {
    value.and_then(Value::as_usize).unwrap_or(0)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A commit id, which dffy puts in revisions: hex only, so a plugin can't
/// name anything else there.
fn commit(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| s.len() >= 4 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned)
}

/// A review's `id` and optional `label`. The id goes on the plugin's
/// command line, so it can't pass for a flag; both show, so they're one
/// printable line.
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

fn session(value: &Value) -> Result<Session, String> {
    let comments =
        PathBuf::from(text(value, "comments").ok_or("the session has no comments file")?);
    if !comments.is_absolute() {
        return Err("the session's comments file isn't an absolute path".to_owned());
    }
    let side = |key: &str| {
        value
            .get(key)
            .and_then(commit)
            .ok_or_else(|| format!("the session's {key} isn't a commit"))
    };
    let (id, label) = review_id(value)?;
    Ok(Session {
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
        comments,
    })
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
    })
}

/// The command for `program`'s `args`, detached from the terminal, in the
/// working tree's `root`, told the comments file for the verbs that take
/// a review.
pub(crate) fn command(
    program: &Path,
    args: &[String],
    root: &Path,
    comments: Option<&Path>,
) -> io::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(&exe);
    command
        .arg(DETACH)
        .arg(program)
        .args(args)
        .current_dir(root)
        .env("DFFY", exe)
        .env("DFFY_REVIEW_PROTOCOL", PROTOCOL.to_string());
    match comments {
        Some(path) => command.env("DFFY_COMMENTS", path),
        None => command.env_remove("DFFY_COMMENTS"),
    };
    Ok(command)
}

#[derive(Debug)]
pub(crate) struct Run {
    leader: Pid,
}

impl Run {
    /// Kills the run with everything it started, unless it's over.
    pub(crate) fn kill(&self) {
        let running = running();
        if running.groups.contains(&self.leader) {
            kill_group(self.leader);
            // Before the detached run leaves dffy's group, it has none.
            rustix::process::kill_process(self.leader, rustix::process::Signal::KILL).ok();
        }
    }
}

/// Starts `command`, a review plugin's verb (`name` and `verb` word the
/// messages), and hands each line it says to `deliver` from a thread of
/// its own, then [`Said::Done`]. Once `deliver` returns false, the run is
/// killed. stdin is closed; stderr's last line goes in a failure's message.
pub(crate) fn start(
    mut command: Command,
    name: &str,
    verb: &str,
    mut deliver: impl FnMut(Said) -> bool + Send + 'static,
) -> io::Result<Run> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let leader = Pid::from_child(&child);
    {
        let mut running = running();
        if running.stopped {
            drop(running);
            super::kill(&mut child);
            return Err(io::Error::other("dffy is exiting"));
        }
        running.groups.push(leader);
    }
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        super::kill(&mut child);
        return Err(io::Error::other("no pipes"));
    };
    let last_said = match std::thread::Builder::new()
        .name("dffy-review-err".into())
        .spawn(move || last_line(stderr))
    {
        Ok(thread) => thread,
        Err(e) => {
            super::kill(&mut child);
            return Err(e);
        }
    };
    let run = Run { leader };
    let (name, verb) = (name.to_owned(), verb.to_owned());
    let reading = std::thread::Builder::new()
        .name("dffy-review".into())
        .spawn(move || {
            let mut said_error = None;
            let mut failure = None;
            for line in BufReader::new(stdout).split(b'\n') {
                let Ok(line) = line else { break };
                if line.trim_ascii().is_empty() {
                    continue;
                }
                match decode(&line) {
                    Ok(Some(Line::Said(said))) => {
                        if !deliver(said) {
                            kill_group(leader);
                            break;
                        }
                    }
                    Ok(Some(Line::Error(message))) => said_error = Some(message),
                    Ok(None) => {}
                    Err(e) => {
                        failure = Some(format!("{name} {verb}: {e}"));
                        kill_group(leader);
                        break;
                    }
                }
            }
            let status = reap(&mut child, leader);
            let stderr = last_said.join().ok().flatten();
            let result = match (failure.or(said_error), status) {
                (Some(message), _) => Err(message),
                (None, Some(status)) if status.success() => Ok(()),
                (None, status) => Err(exited(&name, &verb, status, stderr)),
            };
            deliver(Said::Done(result));
        });
    if let Err(e) = reading {
        run.kill();
        return Err(e);
    }
    Ok(run)
}

/// Waits for the run to exit, then kills what it left in its group and
/// reaps it in one step under the list's lock, so [`super::stop_all`]
/// never signals a reused id.
fn reap(child: &mut std::process::Child, leader: Pid) -> Option<ExitStatus> {
    let exited = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
    while let Err(rustix::io::Errno::INTR) = rustix::process::waitid(WaitId::Pid(leader), exited) {}
    let mut running = running();
    kill_group(leader);
    running.groups.retain(|&g| g != leader);
    child.wait().ok()
}

/// The last line a stream holds that isn't blank, read to its end.
fn last_line(stream: impl Read) -> Option<String> {
    let mut last = None;
    for line in BufReader::new(stream).split(b'\n') {
        let Ok(line) = line else { break };
        let line = String::from_utf8_lossy(line.trim_ascii());
        if !line.is_empty() {
            last = Some(line.chars().take(200).collect());
        }
    }
    last
}

/// Why a run that printed no error failed, from how it exited.
fn exited(name: &str, verb: &str, status: Option<ExitStatus>, stderr: Option<String>) -> String {
    let said = stderr.map(|s| format!(": {s}")).unwrap_or_default();
    match status.and_then(|s| s.code()) {
        Some(2) => format!("{name} doesn't know {verb}{said}"),
        Some(code) => format!("{name} {verb} failed with status {code}{said}"),
        None => format!("{name} {verb} was killed{said}"),
    }
}

#[cfg(test)]
mod tests;
