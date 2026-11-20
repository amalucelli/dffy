use std::io::BufRead;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use super::*;
use crate::plugin::json::parse;

const WAIT: Duration = Duration::from_secs(5);

/// A server on a socket of its own, serving `owner`, and how many views it
/// last said are registered.
fn start(owner: u32) -> (tempfile::TempDir, std::path::PathBuf, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("c.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let views = Arc::new(AtomicUsize::new(usize::MAX));
    let told = Arc::clone(&views);
    thread::spawn(move || serve(&listener, owner, move |n| told.store(n, Ordering::SeqCst)));
    (dir, path, views)
}

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

struct Peer {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Peer {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).expect("connect");
        stream.set_read_timeout(Some(WAIT)).expect("timeout");
        Self {
            writer: stream.try_clone().expect("clone"),
            reader: BufReader::new(stream),
        }
    }

    /// Connected and past the handshake.
    fn open(path: &Path) -> Self {
        let mut peer = Self::connect(path);
        peer.send(r#"{"version":1}"#);
        assert!(peer.recv().get("capabilities").is_some());
        peer
    }

    fn send(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .expect("send");
    }

    /// The next line, `None` once the server hung up.
    fn next(&mut self) -> Option<Value> {
        let mut line = Vec::new();
        match self.reader.read_until(b'\n', &mut line) {
            Ok(0) => None,
            Ok(_) => Some(parse(&line).expect("JSON")),
            Err(e) => panic!("read: {e}"),
        }
    }

    fn recv(&mut self) -> Value {
        self.next().expect("a line")
    }

    fn register(path: &Path) -> Self {
        let mut viewer = Self::open(path);
        viewer.send(
            r#"{"id":1,"method":"views/register","params":{"pid":42,"worktree":"/w","repository":"/w/.git","diff":"main..HEAD","later":true}}"#,
        );
        assert_eq!(
            viewer.recv().get("result").and_then(|r| r.get("view")),
            Some(&number(1))
        );
        viewer
    }
}

fn strings(value: Option<&Value>) -> Vec<&str> {
    value
        .and_then(Value::as_array)
        .expect("an array")
        .iter()
        .map(|v| v.as_str().expect("a string"))
        .collect()
}

#[test]
fn a_hello_of_this_version_gets_the_capabilities_and_another_is_refused() {
    let (_dir, path, _) = start(me());
    let mut peer = Peer::connect(&path);
    peer.send(r#"{"version":1,"client":"test","unknown":{"ignored":true}}"#);
    let welcome = peer.recv();
    assert_eq!(welcome.get("version"), Some(&number(1)));
    assert_eq!(welcome.get("server").and_then(Value::as_str), Some("dffyd"));
    let caps = welcome.get("capabilities").expect("capabilities");
    assert_eq!(strings(caps.get("methods")), METHODS);
    assert_eq!(strings(caps.get("events")), EVENTS);

    let mut old = Peer::connect(&path);
    old.send(r#"{"version":2}"#);
    let refused = old.recv();
    assert_eq!(
        refused
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str),
        Some("unsupported_version")
    );
    assert_eq!(
        refused.get("versions"),
        Some(&Value::Array(vec![number(1)]))
    );
    assert_eq!(old.next(), None, "it hangs up");
}

/// Registering is the connection: closing it unregisters, and so does the
/// viewer dying, which closes it too.
#[test]
fn a_registered_view_is_listed_until_its_connection_closes() {
    let (_dir, path, views) = start(me());
    let viewer = Peer::register(&path);
    assert_eq!(views.load(Ordering::SeqCst), 1);
    let mut client = Peer::open(&path);
    client.send(r#"{"id":"l","method":"views/list"}"#);
    let listed = client.recv();
    let view = &listed
        .get("result")
        .and_then(|r| r.get("views"))
        .and_then(Value::as_array)
        .expect("views")[0];
    assert_eq!(view.get("pid"), Some(&number(42)));
    assert_eq!(view.get("worktree").and_then(Value::as_str), Some("/w"));
    assert_eq!(view.get("later"), None, "only what's known is kept");

    drop(viewer);
    let start = Instant::now();
    loop {
        client.send(r#"{"id":"l","method":"views/list"}"#);
        let listed = client.recv();
        let views = listed
            .get("result")
            .and_then(|r| r.get("views"))
            .and_then(Value::as_array);
        if views.is_some_and(<[Value]>::is_empty) {
            break;
        }
        assert!(start.elapsed() < WAIT, "still listed");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(views.load(Ordering::SeqCst), 0);
}

#[test]
fn requests_reach_their_view_and_answers_come_back_with_its_id() {
    let (_dir, path, _) = start(me());
    let mut viewer = Peer::register(&path);
    let mut client = Peer::open(&path);
    client.send(r#"{"id":"a","method":"view/state","params":{"view":1}}"#);
    let asked = viewer.recv();
    assert_eq!(
        asked.get("method").and_then(Value::as_str),
        Some("view/state")
    );
    let forward = crate::plugin::json::write;
    let mut id = String::new();
    forward(&mut id, asked.get("id").expect("id"));
    viewer.send(&format!(
        r#"{{"id":{id},"result":{{"path":"x.rs","view":9}}}}"#
    ));
    let answer = client.recv();
    assert_eq!(answer.get("id").and_then(Value::as_str), Some("a"));
    let result = answer.get("result").expect("result");
    assert_eq!(result.get("view"), Some(&number(1)), "the view's own id");
    assert_eq!(result.get("path").and_then(Value::as_str), Some("x.rs"));

    let code = |answer: &Value| {
        answer
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    client.send(r#"{"id":2,"method":"view/open","params":{"view":7,"path":"x"}}"#);
    assert_eq!(code(&client.recv()).as_deref(), Some("no_view"));
    client.send(r#"{"id":3,"method":"view/zoom","params":{"view":1}}"#);
    assert_eq!(code(&client.recv()).as_deref(), Some("unknown_method"));
    client.send("not json");
    assert_eq!(code(&client.recv()).as_deref(), Some("parse_error"));
}

#[test]
fn a_view_dying_fails_what_it_was_asked() {
    let (_dir, path, _) = start(me());
    let mut viewer = Peer::register(&path);
    let mut client = Peer::open(&path);
    client.send(r#"{"id":5,"method":"view/selection","params":{"view":1}}"#);
    viewer.recv();
    drop(viewer);
    let answer = client.recv();
    assert_eq!(answer.get("id"), Some(&number(5)));
    assert_eq!(
        answer
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str),
        Some("viewer_gone")
    );
}

#[test]
fn subscribers_get_their_view_s_events_and_the_viewer_hears_who_wants_what() {
    let (_dir, path, _) = start(me());
    let mut viewer = Peer::register(&path);
    let mut client = Peer::open(&path);
    client.send(
        r#"{"id":1,"method":"subscribe","params":{"view":1,"events":["selection/changed"]}}"#,
    );
    assert!(client.recv().get("result").is_some());
    let told = viewer.recv();
    assert_eq!(
        told.get("method").and_then(Value::as_str),
        Some("subscriptions")
    );
    assert_eq!(
        strings(told.get("params").and_then(|p| p.get("events"))),
        ["selection/changed"]
    );

    viewer.send(r#"{"method":"view/changed","params":{"path":"x"}}"#);
    viewer.send(r#"{"method":"selection/changed","params":{"selection":null}}"#);
    let pushed = client.recv();
    assert_eq!(
        pushed.get("method").and_then(Value::as_str),
        Some("selection/changed")
    );
    let params = pushed.get("params").expect("params");
    assert_eq!(params.get("view"), Some(&number(1)));
    assert_eq!(params.get("selection"), Some(&Value::Null));

    let mut other = Peer::open(&path);
    other.send(r#"{"id":1,"method":"subscribe","params":{"view":1,"events":["view/changed"]}}"#);
    other.recv();
    let told = viewer.recv();
    assert_eq!(
        strings(told.get("params").and_then(|p| p.get("events"))),
        EVENTS
    );
    drop(client);
    let told = viewer.recv();
    assert_eq!(
        strings(told.get("params").and_then(|p| p.get("events"))),
        ["view/changed"]
    );
    drop(viewer);
    let closed = other.recv();
    assert_eq!(
        closed.get("method").and_then(Value::as_str),
        Some("view/closed")
    );
}

/// The socket's private directory keeps other users out; this is the
/// second lock, which only Linux has a safe call for.
#[cfg(target_os = "linux")]
#[test]
fn another_users_connection_is_dropped() {
    let (_dir, path, _) = start(me() + 1);
    let mut peer = Peer::connect(&path);
    assert_eq!(peer.next(), None);
}

/// A second subscriber to an event the viewer pushes already gets its
/// last push at once, and the viewer isn't told anything new.
#[test]
fn a_later_subscriber_gets_the_last_push() {
    let (_dir, path, _) = start(me());
    let mut viewer = Peer::register(&path);
    let subscribe =
        r#"{"id":1,"method":"subscribe","params":{"view":1,"events":["selection/changed"]}}"#;
    let mut first = Peer::open(&path);
    first.send(subscribe);
    assert!(first.recv().get("result").is_some());
    assert_eq!(
        viewer.recv().get("method").and_then(Value::as_str),
        Some("subscriptions")
    );
    viewer.send(r#"{"method":"selection/changed","params":{"selection":null}}"#);
    let pushed = first.recv();
    assert_eq!(
        pushed.get("method").and_then(Value::as_str),
        Some("selection/changed")
    );

    let mut second = Peer::open(&path);
    second.send(subscribe);
    assert!(second.recv().get("result").is_some(), "the answer first");
    assert_eq!(second.recv(), pushed, "then the last push");
    first.send(r#"{"id":2,"method":"view/state","params":{"view":1}}"#);
    assert_eq!(
        viewer.recv().get("method").and_then(Value::as_str),
        Some("view/state"),
        "no new subscriptions came before it"
    );
}
