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

use rustix::process::{Pid, WaitId, WaitIdOptions};

use super::json::{self, Value};
use super::{kill_group, running};

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

/// A PR under review, from a session line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub number: u64,
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

/// Why `list` lists a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    CurrentBranch,
    ReviewRequested,
    Mine,
    /// A reason this dffy doesn't know.
    Other,
}

/// A PR from `list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pr {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub reason: Reason,
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
    /// `detect`: the branch checked out's PR, as a session, if it has one.
    Detected(Option<Session>),
    Progress(String),
    Prs(Vec<Pr>),
    /// `check`: the PR's head on the host, and whether it isn't the
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
    /// `post` finished: comments posted and not, whether your review is
    /// still pending on the host, and the review's link.
    Posted {
        posted: usize,
        failed: usize,
        pending_review: bool,
        url: Option<String>,
    },
    /// The run is over; `Err` says why it failed: the plugin's own error
    /// line, else how it exited.
    Done(Result<(), String>),
}

/// A line the plugin printed.
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
    } else if let Some(pr) = value.get("pr") {
        Said::Detected(match pr {
            Value::Null => None,
            pr => Some(session(pr)?),
        })
    } else if let Some(p) = value.get("progress") {
        Said::Progress(p.as_str().ok_or("progress isn't text")?.to_owned())
    } else if let Some(prs) = value.get("prs") {
        let prs = prs.as_array().ok_or("prs isn't a list")?;
        Said::Prs(prs.iter().map(pr).collect::<Result<_, _>>()?)
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
            pending_review: value.get("pending_review") == Some(&Value::Bool(true)),
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

fn number(value: &Value) -> Result<u64, String> {
    value
        .get("number")
        .and_then(Value::as_usize)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| "no PR number".to_owned())
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
    Ok(Session {
        number: number(value)?,
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

fn pr(value: &Value) -> Result<Pr, String> {
    Ok(Pr {
        number: number(value)?,
        title: text(value, "title").unwrap_or_default(),
        author: text(value, "author").unwrap_or_default(),
        reason: match value.get("reason").and_then(Value::as_str) {
            Some("current_branch") => Reason::CurrentBranch,
            Some("review_requested") => Reason::ReviewRequested,
            Some("mine") => Reason::Mine,
            _ => Reason::Other,
        },
    })
}

/// The command for `program`'s `args`, detached from the terminal, in the
/// working tree's `root`, told the comments file for the verbs that take
/// a PR.
pub(crate) fn command(
    program: &Path,
    args: &[String],
    root: &Path,
    comments: Option<&Path>,
) -> io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(DETACH)
        .arg(program)
        .args(args)
        .current_dir(root)
        .env("DFFY_REVIEW_PROTOCOL", PROTOCOL.to_string());
    match comments {
        Some(path) => command.env("DFFY_COMMENTS", path),
        None => command.env_remove("DFFY_COMMENTS"),
    };
    Ok(command)
}

/// A run started by [`start`].
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
    let last_said = std::thread::Builder::new()
        .name("dffy-review-err".into())
        .spawn(move || last_line(stderr));
    let last_said = match last_said {
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
