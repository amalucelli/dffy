//! A viewer's registration with dffyd: its connection, held for the
//! viewer's life, on threads of its own. Requests come in parsed, as
//! [`Command`]s with the [`Reply`] that answers them; the viewer's loop
//! answers when it's done, and a writer thread sends it, so the loop never
//! waits on the socket. A viewer with nobody subscribed to its events pushes
//! nothing.

use std::collections::HashMap;
use std::io::{self, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread;
use std::time::Duration;

use dffy_core::document::Side;

use super::{Fault, VERSION, answer, line, notification, number, object, text};
use crate::encode;
use crate::plugin::json::Value;

/// How long dffyd gets to answer the hello and the registration.
const HANDSHAKE: Duration = Duration::from_secs(3);

/// What a viewer registers as, besides its pid.
pub(crate) struct Info {
    pub worktree: PathBuf,
    /// The git directory the repository's worktrees share.
    pub repository: PathBuf,
    /// What the view compares, as its title says.
    pub diff: String,
}

pub(crate) enum Inbound {
    Command(Command, Reply),
    /// What's subscribed to now: push those events through `push`.
    Wanted {
        selection: bool,
        view: bool,
        push: Push,
    },
    /// The viewer couldn't register, for this reason: nothing can drive it.
    Unreachable(String),
    /// A request's asker left, which [`Reply::cancelled`] now says: what
    /// waits on it goes undone.
    Cancelled,
}

/// A request to the view, its lines 0-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    State,
    Selection,
    Open {
        path: String,
        line: Option<usize>,
        side: Side,
    },
    Focus {
        id: String,
    },
    Comment {
        path: String,
        line: usize,
        side: Side,
        text: String,
        /// The client's name from its hello, else `dffy ctl`.
        author: String,
    },
}

/// Who a control comment is by when its client gave no name.
const UNNAMED: &str = "dffy ctl";

/// What `view/state` answers and `view/changed` pushes, its lines 0-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State {
    /// The file shown, unless it's none or a commit's page.
    pub path: Option<String>,
    /// A renamed file's path before.
    pub old_path: Option<String>,
    /// The top row's line on each side, old then new.
    pub top: [Option<usize>; 2],
    pub files: usize,
    /// The files are still being listed.
    pub listing: bool,
    pub split: bool,
    pub diff: String,
    pub revisions: Vec<String>,
}

impl State {
    pub(crate) fn encode(&self) -> Value {
        let optional = |s: &Option<String>| s.as_ref().map_or(Value::Null, |s| text(s.clone()));
        let [old, new] = self.top;
        object(vec![
            ("path", optional(&self.path)),
            ("old_path", optional(&self.old_path)),
            (
                "top",
                object(vec![("old", one_based(old)), ("new", one_based(new))]),
            ),
            ("files", number(self.files as u64)),
            ("listing", Value::Bool(self.listing)),
            ("layout", text(if self.split { "split" } else { "unified" })),
            ("diff", text(self.diff.clone())),
            (
                "revisions",
                Value::Array(self.revisions.iter().cloned().map(text).collect()),
            ),
        ])
    }
}

/// The lines selected in the diff, 0-based, and their text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub path: String,
    pub side: Side,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// `selection`, or null when nothing is selected.
pub(crate) fn selection(selection: Option<&Selection>) -> Value {
    let Some(s) = selection else {
        return Value::Null;
    };
    object(vec![
        ("path", text(s.path.clone())),
        ("side", text(encode::side(s.side))),
        ("start", one_based(Some(s.start))),
        ("end", one_based(Some(s.end))),
        ("text", text(s.text.clone())),
    ])
}

/// Where a `view/open` went, or the comment a `comments/add` added there,
/// the line 0-based.
pub(crate) fn shown(id: Option<String>, path: String, at: Option<usize>, side: Side) -> Value {
    let id = id.map(|id| ("id", text(id)));
    let at = [
        ("path", text(path)),
        ("line", one_based(at)),
        ("side", text(encode::side(side))),
    ];
    object(id.into_iter().chain(at).collect())
}

/// What `view/focusThread` answers.
pub(crate) fn focused(thread: String, path: String, focused: bool) -> Value {
    object(vec![
        ("thread", text(thread)),
        ("path", text(path)),
        ("focused", Value::Bool(focused)),
    ])
}

/// A 0-based line, 1-based on the wire.
fn one_based(line: Option<usize>) -> Value {
    line.map_or(Value::Null, |l| number(l as u64 + 1))
}

impl Command {
    /// `method` with `params`, from the client dffyd names `client`.
    fn parse(method: &str, params: Option<&Value>, client: Option<&str>) -> Result<Self, Fault> {
        let get = |key: &str| {
            params
                .and_then(|p| p.get(key))
                .filter(|v| **v != Value::Null)
        };
        let string = |key: &str| match get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
            _ => Err(Fault::invalid(format!("{key} must be a string, not empty"))),
        };
        let line = || match get("line") {
            None => Ok(None),
            Some(n) => n
                .as_usize()
                .and_then(|n| n.checked_sub(1))
                .map(Some)
                .ok_or_else(|| Fault::invalid("line must be a whole number from 1")),
        };
        let side = || match get("side").map(Value::as_str) {
            None | Some(Some("new")) => Ok(Side::New),
            Some(Some("old")) => Ok(Side::Old),
            Some(_) => Err(Fault::invalid("side must be old or new")),
        };
        Ok(match method {
            "view/state" => Self::State,
            "view/selection" => Self::Selection,
            "view/open" => Self::Open {
                path: string("path")?,
                line: line()?,
                side: side()?,
            },
            "view/focusThread" => Self::Focus { id: string("id")? },
            "comments/add" => Self::Comment {
                path: string("path")?,
                line: line()?.ok_or_else(|| Fault::invalid("line is needed"))?,
                side: side()?,
                text: string("text")?.trim().to_owned(),
                author: client.unwrap_or(UNNAMED).to_owned(),
            },
            _ => {
                return Err(Fault::new(
                    "unknown_method",
                    format!("this viewer doesn't answer {method}"),
                ));
            }
        })
    }
}

/// Answers one request.
pub(crate) struct Reply {
    id: Value,
    tx: Sender<String>,
    /// Set from the connection's thread when dffyd says the asker left.
    cancelled: Arc<AtomicBool>,
}

impl Reply {
    pub(crate) fn send(self, result: Result<Value, Fault>) {
        self.tx.send(answer(&self.id, result)).ok();
    }

    /// Whoever asked left before it was answered, as after giving up
    /// waiting: it's not to be done.
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// Pushes the view's events to their subscribers.
#[derive(Clone)]
pub(crate) struct Push(Sender<String>);

impl Push {
    pub(crate) fn send(&self, event: &str, params: Value) {
        self.0.send(notification(event, params)).ok();
    }
}

/// A live registration; dropping it unregisters the view.
pub(crate) struct Registration {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    stopped: AtomicBool,
    stream: Mutex<Option<UnixStream>>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        let stream = self
            .shared
            .stream
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(stream) = stream {
            stream.shutdown(std::net::Shutdown::Both).ok();
        }
    }
}

/// Registers the view described by `info`, starting dffyd when none
/// serves the control socket, and hands what comes for it to `sink` until
/// that returns false. Returns at once; the rest happens on a thread.
///
/// # Errors
///
/// When the thread can't start.
pub(crate) fn register(
    info: Info,
    sink: impl Fn(Inbound) -> bool + Send + 'static,
) -> io::Result<Registration> {
    let shared = Arc::new(Shared::default());
    let state = Arc::clone(&shared);
    thread::Builder::new()
        .name("dffy-control".into())
        .spawn(move || {
            if let Err(e) = serve(&info, &state, &sink)
                && !state.stopped.load(Ordering::Relaxed)
            {
                sink(Inbound::Unreachable(e.to_string()));
            }
        })?;
    Ok(Registration { shared })
}

/// Connects to dffyd and registers as `info` describes.
fn join(info: &Info) -> io::Result<(UnixStream, BufReader<UnixStream>)> {
    // Canonical, as clients compare them with paths they resolve.
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    let utf8 = |p: PathBuf| {
        p.into_os_string()
            .into_string()
            .map_err(|p| io::Error::other(format!("{} isn't UTF-8", Path::new(&p).display())))
    };
    let (worktree, repository) = (
        utf8(canonical(&info.worktree))?,
        utf8(canonical(&info.repository))?,
    );
    let mut stream = crate::daemon::client::connect_or_spawn(super::socket_path)?;
    stream.set_read_timeout(Some(HANDSHAKE))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut buf = Vec::new();
    let mut ask = |stream: &mut UnixStream, message: Value| -> io::Result<()> {
        stream.write_all(line(&message).as_bytes())?;
        let answer = match super::read(&mut reader, &mut buf)? {
            Some(Ok(value)) => value,
            Some(Err(e)) => return Err(io::Error::other(e)),
            None => return Err(io::ErrorKind::UnexpectedEof.into()),
        };
        match answer.get("error") {
            Some(error) => Err(io::Error::other(
                (error.get("message").and_then(Value::as_str)).unwrap_or("dffyd refused"),
            )),
            None => Ok(()),
        }
    };
    let hello = object(vec![
        ("version", number(VERSION as u64)),
        ("client", text(format!("dffy {}", crate::BUILD))),
    ]);
    ask(&mut stream, hello)?;
    let params = object(vec![
        ("pid", number(u64::from(std::process::id()))),
        ("worktree", text(worktree)),
        ("repository", text(repository)),
        ("diff", text(info.diff.clone())),
    ]);
    let register = object(vec![
        ("id", number(1)),
        ("method", text("views/register")),
        ("params", params),
    ]);
    ask(&mut stream, register)?;
    stream.set_read_timeout(None)?;
    Ok((stream, reader))
}

fn serve(info: &Info, shared: &Shared, sink: &dyn Fn(Inbound) -> bool) -> io::Result<()> {
    let (stream, mut reader) = join(info)?;
    {
        let mut held = shared.stream.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.stopped.load(Ordering::Relaxed) {
            return Ok(());
        }
        *held = Some(stream.try_clone()?);
    }
    let (tx, rx) = mpsc::channel::<String>();
    let mut writer = stream;
    thread::Builder::new()
        .name("dffy-control-write".into())
        .spawn(move || {
            for line in rx {
                if writer.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
        })?;
    let mut buf = Vec::new();
    // The requests handed to the loop and not answered yet, by dffyd's id,
    // for it to call off.
    let mut asked: HashMap<u64, Weak<AtomicBool>> = HashMap::new();
    loop {
        let message = match super::read(&mut reader, &mut buf) {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) => continue,
            Ok(None) | Err(_) => return Ok(()),
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let client = message.get("client").and_then(Value::as_str);
        let inbound = match message.get("id") {
            Some(id) => match Command::parse(method, message.get("params"), client) {
                Ok(command) => {
                    let cancelled = Arc::new(AtomicBool::new(false));
                    asked.retain(|_, c| c.strong_count() > 0);
                    if let Some(id) = id.as_usize() {
                        asked.insert(id as u64, Arc::downgrade(&cancelled));
                    }
                    Inbound::Command(
                        command,
                        Reply {
                            id: id.clone(),
                            tx: tx.clone(),
                            cancelled,
                        },
                    )
                }
                Err(fault) => {
                    tx.send(answer(id, Err(fault))).ok();
                    continue;
                }
            },
            None if method == "$/cancelRequest" => {
                let id = message.get("params").and_then(|p| p.get("id"));
                let Some(cancelled) = (id.and_then(Value::as_usize))
                    .and_then(|id| asked.remove(&(id as u64)))
                    .and_then(|c| c.upgrade())
                else {
                    continue;
                };
                cancelled.store(true, Ordering::Relaxed);
                Inbound::Cancelled
            }
            None if method == "subscriptions" => {
                let events = message.get("params").and_then(|p| p.get("events"));
                let wants = |event: &str| {
                    events
                        .and_then(Value::as_array)
                        .is_some_and(|e| e.iter().any(|v| v.as_str() == Some(event)))
                };
                Inbound::Wanted {
                    selection: wants("selection/changed"),
                    view: wants("view/changed"),
                    push: Push(tx.clone()),
                }
            }
            None => continue,
        };
        if !sink(inbound) {
            return Ok(());
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::json::parse;

    /// The fields in the order docs/control.md lists them, lines 1-based.
    #[test]
    fn answers_encode_as_the_protocol_says() {
        let state = State {
            path: Some("src/b.rs".into()),
            old_path: Some("src/a.rs".into()),
            top: [None, Some(41)],
            files: 3,
            listing: false,
            split: true,
            diff: "main...HEAD".into(),
            revisions: vec!["main...HEAD".into()],
        };
        assert_eq!(
            line(&state.encode()),
            concat!(
                r#"{"path":"src/b.rs","old_path":"src/a.rs","top":{"old":null,"new":42},"#,
                r#""files":3,"listing":false,"layout":"split","diff":"main...HEAD","#,
                r#""revisions":["main...HEAD"]}"#,
                "\n"
            )
        );
        let selected = Selection {
            path: "a.rs".into(),
            side: Side::Old,
            start: 0,
            end: 1,
            text: "x\n".into(),
        };
        assert_eq!(
            line(&selection(Some(&selected))),
            "{\"path\":\"a.rs\",\"side\":\"old\",\"start\":1,\"end\":2,\"text\":\"x\\n\"}\n"
        );
        assert_eq!(selection(None), Value::Null);
        assert_eq!(
            line(&shown(Some("c1".into()), "a.rs".into(), Some(9), Side::New)),
            "{\"id\":\"c1\",\"path\":\"a.rs\",\"line\":10,\"side\":\"new\"}\n"
        );
        assert_eq!(
            line(&shown(None, "a.rs".into(), None, Side::New)),
            "{\"path\":\"a.rs\",\"line\":null,\"side\":\"new\"}\n"
        );
    }

    fn parse_command(method: &str, params: &str) -> Result<Command, Fault> {
        Command::parse(method, Some(&parse(params.as_bytes()).expect("JSON")), None)
    }

    /// Lines come 1-based and go 0-based; the side is new unless given,
    /// and fields the viewer doesn't know are ignored.
    #[test]
    fn requests_parse_into_commands() {
        assert_eq!(
            parse_command(
                "view/open",
                r#"{"view":1,"path":"a.rs","line":42,"side":"old","x":1}"#
            ),
            Ok(Command::Open {
                path: "a.rs".into(),
                line: Some(41),
                side: Side::Old
            })
        );
        assert_eq!(
            parse_command("view/open", r#"{"path":"a.rs"}"#),
            Ok(Command::Open {
                path: "a.rs".into(),
                line: None,
                side: Side::New
            })
        );
        assert_eq!(
            parse_command(
                "comments/add",
                r#"{"path":"a.rs","line":1,"text":" fix \n"}"#
            ),
            Ok(Command::Comment {
                path: "a.rs".into(),
                line: 0,
                side: Side::New,
                text: "fix".into(),
                author: "dffy ctl".into(),
            })
        );
        let named = Command::parse(
            "comments/add",
            Some(&parse(br#"{"path":"a.rs","line":1,"text":"fix"}"#).expect("JSON")),
            Some("claude"),
        );
        assert!(
            matches!(named, Ok(Command::Comment { author, .. }) if author == "claude"),
            "by the client's name"
        );
        for (method, params) in [
            ("view/open", r#"{"path":"a.rs","line":0}"#),
            ("view/open", r#"{"path":"a.rs","side":"left"}"#),
            ("comments/add", r#"{"path":"a.rs","text":"no line"}"#),
            ("comments/add", r#"{"path":"a.rs","line":3,"text":"  "}"#),
            ("view/focusThread", r"{}"),
        ] {
            let fault = parse_command(method, params).expect_err(params);
            assert_eq!(fault.code, "invalid_params", "{params}");
        }
    }
}
