//! dffyd's side of the control API: who's registered, and requests for a
//! view handed to its viewer and answered back.
//!
//! A viewer's registration is its connection: closing it, or the viewer
//! dying, unregisters it. Each connection gets a thread reading it and one
//! writing to it from an unbounded queue, so a slow peer never holds up the
//! others; one that can't take a write within a second is dropped. Nothing
//! here wakes up on its own: every thread waits on a read or a queue.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufReader, Write};
use std::ops::Range;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use super::{EVENTS, Fault, METHODS, VERSION, answer, line, notification, number, object, text};
use crate::plugin::json::Value;

const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a connection may stay silent before its hello: one that never
/// sends it would hold a thread for good.
const HELLO_TIMEOUT: Duration = Duration::from_secs(60);
/// The wait after a failed accept, doubling to the end while they fail.
const ACCEPT_BACKOFF: Range<Duration> = Duration::from_millis(10)..Duration::from_secs(1);

/// The requests dffyd hands to the view they name.
const FORWARDED: [&str; 5] = [
    "view/state",
    "view/selection",
    "view/open",
    "view/focusThread",
    "comments/add",
];

type Views = Box<dyn Fn(usize) + Send + Sync>;

struct Server {
    /// The only user whose connections are served.
    owner: u32,
    registry: Mutex<Registry>,
    /// Told how many views are registered whenever that changes.
    on_views: Views,
}

#[derive(Default)]
struct Registry {
    next_conn: u64,
    next_view: u64,
    next_forward: u64,
    views: BTreeMap<u64, View>,
    /// Requests handed to a view, by the id they went to it with.
    waiting: HashMap<u64, Waiting>,
}

struct View {
    conn: u64,
    tx: Sender<String>,
    /// As `views/list` lists it.
    info: Value,
    subscribers: Vec<Subscriber>,
    /// The events the viewer was last told someone subscribes to.
    told: Vec<&'static str>,
    /// The last push of each event, for a subscriber that comes after the
    /// viewer pushed what's there.
    last: Vec<(&'static str, String)>,
}

struct Subscriber {
    conn: u64,
    tx: Sender<String>,
    events: Vec<&'static str>,
}

/// A request a view is to answer: whose, and the id it came with.
struct Waiting {
    view: u64,
    conn: u64,
    tx: Sender<String>,
    id: Value,
}

impl View {
    /// The events any subscriber wants, in [`EVENTS`] order.
    fn wanted(&self) -> Vec<&'static str> {
        EVENTS
            .into_iter()
            .filter(|e| self.subscribers.iter().any(|s| s.events.contains(e)))
            .collect()
    }

    /// Tells the viewer what's subscribed to, when that changed.
    fn tell(&mut self) {
        let wanted = self.wanted();
        if wanted != self.told {
            let events = Value::Array(wanted.iter().map(|e| text(*e)).collect());
            self.tx
                .send(notification(
                    "subscriptions",
                    object(vec![("events", events)]),
                ))
                .ok();
            self.told = wanted;
        }
    }
}

/// Serves `listener` until the process exits, to connections of user
/// `owner` only; `on_views` hears how many views are registered whenever
/// that changes.
pub(crate) fn serve(
    listener: &UnixListener,
    owner: u32,
    on_views: impl Fn(usize) + Send + Sync + 'static,
) {
    let server = Arc::new(Server {
        owner,
        registry: Mutex::default(),
        on_views: Box::new(on_views),
    });
    let mut backoff = ACCEPT_BACKOFF.start;
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            // Out of descriptors, most likely: retrying at once would spin
            // until a connection closes.
            thread::sleep(backoff);
            backoff = (backoff * 2).min(ACCEPT_BACKOFF.end);
            continue;
        };
        backoff = ACCEPT_BACKOFF.start;
        if !owned_by(&stream, server.owner) {
            continue;
        }
        let server = Arc::clone(&server);
        drop(
            thread::Builder::new()
                .name("dffyd-control".into())
                .spawn(move || server.connection(stream)),
        );
    }
}

/// Whether the connecting process is user `owner`'s; refused when that
/// can't be told. The socket's directory is private, so only its owner can
/// connect anyway; on systems other than Linux and macOS, that's all there
/// is.
#[cfg(target_os = "linux")]
fn owned_by(stream: &UnixStream, owner: u32) -> bool {
    rustix::net::sockopt::socket_peercred(stream).is_ok_and(|cred| cred.uid.as_raw() == owner)
}

#[cfg(target_os = "macos")]
fn owned_by(stream: &UnixStream, owner: u32) -> bool {
    nix::unistd::getpeereid(stream).is_ok_and(|(uid, _)| uid.as_raw() == owner)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn owned_by(_: &UnixStream, _: u32) -> bool {
    true
}

fn welcome() -> String {
    let list = |names: &[&str]| Value::Array(names.iter().map(|n| text(*n)).collect());
    line(&object(vec![
        ("version", number(VERSION as u64)),
        ("server", text("dffyd")),
        ("build", text(crate::BUILD)),
        (
            "capabilities",
            object(vec![("methods", list(&METHODS)), ("events", list(&EVENTS))]),
        ),
    ]))
}

fn missing(view: u64) -> Fault {
    Fault::new(
        "no_view",
        format!("no view {view} is registered; views/list lists them"),
    )
}

fn refused(message: &str) -> String {
    line(&object(vec![
        (
            "error",
            super::fault_value(&Fault::new("unsupported_version", message)),
        ),
        ("versions", Value::Array(vec![number(VERSION as u64)])),
    ]))
}

impl Server {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn connection(&self, stream: UnixStream) {
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        if writer.set_write_timeout(Some(WRITE_TIMEOUT)).is_err() {
            return;
        }
        let (tx, rx) = mpsc::channel::<String>();
        let wrote = thread::Builder::new()
            .name("dffyd-control-write".into())
            .spawn(move || {
                for line in rx {
                    if writer.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                }
                // Ends the read below too.
                writer.shutdown(std::net::Shutdown::Both).ok();
            });
        if wrote.is_err() || stream.set_read_timeout(Some(HELLO_TIMEOUT)).is_err() {
            return;
        }
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        let hello = match super::read(&mut reader, &mut buf) {
            Ok(Some(Ok(hello)))
                if hello.get("version").and_then(Value::as_usize) == Some(VERSION) =>
            {
                Ok(hello)
            }
            Ok(Some(Ok(_))) => Err(format!(
                "dffyd speaks version {VERSION} of the control protocol"
            )),
            Ok(Some(Err(e))) => Err(format!("the hello isn't JSON: {e}")),
            Ok(None) | Err(_) => return,
        };
        let hello = match hello {
            Ok(hello) => hello,
            Err(why) => {
                tx.send(refused(&why)).ok();
                return;
            }
        };
        // What the views it asks are told it's called: a comment it adds is
        // by that name.
        let client = (hello.get("client").and_then(Value::as_str))
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| c.chars().take(64).collect::<String>());
        tx.send(welcome()).ok();
        if reader.get_ref().set_read_timeout(None).is_err() {
            return;
        }
        let conn = {
            let mut registry = self.registry();
            registry.next_conn += 1;
            registry.next_conn
        };
        loop {
            match super::read(&mut reader, &mut buf) {
                Ok(Some(Ok(message))) => self.message((conn, client.as_deref()), &tx, &message),
                Ok(Some(Err(e))) => {
                    tx.send(answer(&Value::Null, Err(Fault::new("parse_error", e))))
                        .ok();
                }
                Ok(None) | Err(_) => break,
            }
        }
        self.closed(conn);
    }

    /// A message on connection `conn`, from the client the hello named
    /// `client`.
    fn message(&self, (conn, client): (u64, Option<&str>), tx: &Sender<String>, message: &Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message
            .get("id")
            .filter(|id| matches!(id, Value::Number(_) | Value::String(_)));
        let params = match message.get("params") {
            Some(Value::Object(fields)) => &fields[..],
            _ => &[],
        };
        match (method, id) {
            (Some(method), Some(id)) => {
                if let Some(result) = self.request((conn, client), tx, method, id, params) {
                    tx.send(answer(id, result)).ok();
                }
            }
            (Some(method), None) => self.notified(conn, method, params),
            (None, Some(id))
                if message.get("result").is_some() || message.get("error").is_some() =>
            {
                self.answered(conn, id, message);
            }
            _ => {
                let fault = Fault::new(
                    "invalid_request",
                    "not a request, an answer or a notification",
                );
                tx.send(answer(id.unwrap_or(&Value::Null), Err(fault))).ok();
            }
        }
    }

    /// The answer to a request, `None` when a view is to give it.
    fn request(
        &self,
        (conn, client): (u64, Option<&str>),
        tx: &Sender<String>,
        method: &str,
        id: &Value,
        params: &[(String, Value)],
    ) -> Option<Result<Value, Fault>> {
        let param = |key: &str| params.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        let view = || {
            param("view")
                .and_then(Value::as_usize)
                .map(|v| v as u64)
                .ok_or_else(|| Fault::invalid("view must be a view's id; views/list lists them"))
        };
        Some(match method {
            "views/list" => Ok(object(vec![(
                "views",
                Value::Array(
                    (self.registry().views.values())
                        .map(|v| v.info.clone())
                        .collect(),
                ),
            )])),
            "views/register" => self.register(conn, tx, &param),
            "subscribe" => {
                match view().and_then(|v| self.subscribe(conn, tx, v, param("events"))) {
                    // The answer goes first, then what's there now.
                    Ok((result, pushed)) => {
                        tx.send(answer(id, Ok(result))).ok();
                        for line in pushed {
                            tx.send(line).ok();
                        }
                        return None;
                    }
                    Err(e) => Err(e),
                }
            }
            method if FORWARDED.contains(&method) => {
                let asked = (method, id, params);
                match view().and_then(|view| self.forward((conn, client), tx, view, asked)) {
                    Ok(()) => return None,
                    Err(e) => Err(e),
                }
            }
            _ => Err(Fault::new(
                "unknown_method",
                format!("no method {method}; the welcome lists them"),
            )),
        })
    }

    fn register<'a>(
        &self,
        conn: u64,
        tx: &Sender<String>,
        param: &dyn Fn(&str) -> Option<&'a Value>,
    ) -> Result<Value, Fault> {
        let string = |key: &str| {
            param(key)
                .and_then(Value::as_str)
                .map(text)
                .ok_or_else(|| Fault::invalid(format!("{key} must be a string")))
        };
        let pid = param("pid")
            .and_then(Value::as_usize)
            .ok_or_else(|| Fault::invalid("pid must be a number"))?;
        let (worktree, repository, diff) =
            (string("worktree")?, string("repository")?, string("diff")?);
        let mut registry = self.registry();
        if let Some((id, _)) = registry.views.iter().find(|(_, v)| v.conn == conn) {
            return Err(Fault::new(
                "invalid_request",
                format!("this connection is view {id} already"),
            ));
        }
        registry.next_view += 1;
        let view = registry.next_view;
        let info = object(vec![
            ("view", number(view)),
            ("pid", number(pid as u64)),
            ("worktree", worktree),
            ("repository", repository),
            ("diff", diff),
        ]);
        registry.views.insert(
            view,
            View {
                conn,
                tx: tx.clone(),
                info,
                subscribers: Vec::new(),
                told: Vec::new(),
                last: Vec::new(),
            },
        );
        let count = registry.views.len();
        drop(registry);
        (self.on_views)(count);
        Ok(object(vec![("view", number(view))]))
    }

    fn subscribe(
        &self,
        conn: u64,
        tx: &Sender<String>,
        view: u64,
        events: Option<&Value>,
    ) -> Result<(Value, Vec<String>), Fault> {
        let events: Option<Vec<&'static str>> =
            events.and_then(Value::as_array).and_then(|items| {
                (items.iter())
                    .map(|e| EVENTS.into_iter().find(|known| Some(*known) == e.as_str()))
                    .collect()
            });
        let events = events.filter(|e| !e.is_empty()).ok_or_else(|| {
            Fault::invalid(format!("events must list some of {}", EVENTS.join(", ")))
        })?;
        let mut registry = self.registry();
        let entry = registry.views.get_mut(&view).ok_or_else(|| missing(view))?;
        match entry.subscribers.iter_mut().find(|s| s.conn == conn) {
            Some(s) => {
                for e in &events {
                    if !s.events.contains(e) {
                        s.events.push(e);
                    }
                }
            }
            None => entry.subscribers.push(Subscriber {
                conn,
                tx: tx.clone(),
                events: events.clone(),
            }),
        }
        // Events the viewer pushes already: what it last pushed is what's
        // there. For the others, telling it brings what's there.
        let pushed = (entry.last.iter())
            .filter(|(e, _)| events.contains(e) && entry.told.contains(e))
            .map(|(_, line)| line.clone())
            .collect();
        entry.tell();
        let events = Value::Array(events.iter().map(|e| text(*e)).collect());
        Ok((
            object(vec![("view", number(view)), ("events", events)]),
            pushed,
        ))
    }

    /// Hands request `id` to `view`, to answer when it does, with the name
    /// of the client asking, when it gave one.
    fn forward(
        &self,
        (conn, client): (u64, Option<&str>),
        tx: &Sender<String>,
        view: u64,
        (method, id, params): (&str, &Value, &[(String, Value)]),
    ) -> Result<(), Fault> {
        let mut registry = self.registry();
        registry.next_forward += 1;
        let forward = registry.next_forward;
        let entry = registry.views.get(&view).ok_or_else(|| missing(view))?;
        let mut request = vec![
            ("id", number(forward)),
            ("method", text(method)),
            ("params", Value::Object(params.to_vec())),
        ];
        request.extend(client.map(|c| ("client", text(c))));
        let request = line(&object(request));
        if entry.tx.send(request).is_err() {
            return Err(Fault::new("viewer_gone", "the viewer is leaving"));
        }
        registry.waiting.insert(
            forward,
            Waiting {
                view,
                conn,
                tx: tx.clone(),
                id: id.clone(),
            },
        );
        Ok(())
    }

    /// A view's answer to a request handed to it, passed on to whoever
    /// asked, the view's id added to its result.
    fn answered(&self, conn: u64, id: &Value, message: &Value) {
        let mut registry = self.registry();
        let Some(forward) = id.as_usize().map(|f| f as u64) else {
            return;
        };
        let from_view = registry
            .waiting
            .get(&forward)
            .and_then(|w| registry.views.get(&w.view))
            .is_some_and(|v| v.conn == conn);
        if !from_view {
            return;
        }
        let Some(waiting) = registry.waiting.remove(&forward) else {
            return;
        };
        drop(registry);
        let reply = match (message.get("result"), message.get("error")) {
            (Some(Value::Object(fields)), _) => {
                let mut fields = fields.clone();
                fields.retain(|(k, _)| k != "view");
                fields.insert(0, ("view".to_owned(), number(waiting.view)));
                object(vec![("id", waiting.id), ("result", Value::Object(fields))])
            }
            (_, Some(error)) => object(vec![("id", waiting.id), ("error", error.clone())]),
            (Some(_), None) => object(vec![
                ("id", waiting.id),
                (
                    "error",
                    super::fault_value(&Fault::new(
                        "failed",
                        "the viewer's answer isn't an object",
                    )),
                ),
            ]),
            (None, None) => return,
        };
        waiting.tx.send(line(&reply)).ok();
    }

    /// A notification from a viewer, pushed to the subscribers of its
    /// event with the view's id in its params.
    fn notified(&self, conn: u64, method: &str, params: &[(String, Value)]) {
        let Some(event) = EVENTS.into_iter().find(|e| *e == method) else {
            return;
        };
        let mut registry = self.registry();
        let Some((&view, entry)) = registry.views.iter_mut().find(|(_, v)| v.conn == conn) else {
            return;
        };
        let mut fields = params.to_vec();
        fields.retain(|(k, _)| k != "view");
        fields.insert(0, ("view".to_owned(), number(view)));
        let pushed = notification(event, Value::Object(fields));
        for s in entry
            .subscribers
            .iter()
            .filter(|s| s.events.contains(&event))
        {
            s.tx.send(pushed.clone()).ok();
        }
        entry.last.retain(|(e, _)| *e != event);
        entry.last.push((event, pushed));
    }

    /// Connection `conn` is gone: its view leaves, its requests' answers
    /// are dropped and its subscriptions end.
    fn closed(&self, conn: u64) {
        let mut registry = self.registry();
        let mut left = None;
        if let Some(view) = registry
            .views
            .iter()
            .find(|(_, v)| v.conn == conn)
            .map(|(&id, _)| id)
        {
            let entry = registry.views.remove(&view);
            let gone: Vec<u64> = (registry.waiting.iter())
                .filter(|(_, w)| w.view == view)
                .map(|(&f, _)| f)
                .collect();
            for forward in gone {
                if let Some(w) = registry.waiting.remove(&forward) {
                    let fault = Fault::new("viewer_gone", "the viewer exited before it answered");
                    w.tx.send(answer(&w.id, Err(fault))).ok();
                }
            }
            for s in entry.iter().flat_map(|e| &e.subscribers) {
                s.tx.send(notification(
                    "view/closed",
                    object(vec![("view", number(view))]),
                ))
                .ok();
            }
            left = Some(registry.views.len());
        }
        for view in registry.views.values_mut() {
            view.subscribers.retain(|s| s.conn != conn);
            view.tell();
        }
        // What it asked and gave up waiting for is called off, so a view
        // answering late doesn't do it anyway.
        let asked: Vec<(u64, u64)> = (registry.waiting.iter())
            .filter(|(_, w)| w.conn == conn)
            .map(|(&f, w)| (f, w.view))
            .collect();
        for (forward, view) in asked {
            registry.waiting.remove(&forward);
            if let Some(view) = registry.views.get(&view) {
                let cancel = object(vec![("id", number(forward))]);
                view.tx.send(notification("$/cancelRequest", cancel)).ok();
            }
        }
        drop(registry);
        if let Some(count) = left {
            (self.on_views)(count);
        }
    }
}

#[cfg(test)]
mod tests;
