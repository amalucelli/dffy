//! dffy's side of a long-lived plugin process speaking JSON-RPC 2.0 over
//! its stdin and stdout, one message a line, as review plugins do (see
//! [`super::review`]). A [`Client`] owns what every such kind of plugin
//! shares: starting the process, `initialize` and the disk cache of its
//! answer, request ids, requests waiting for their answer,
//! `$/cancelRequest`, starting the process again when it crashes, and
//! closing it. The kind's [`Kind`] says what `initialize`'s answer,
//! responses and notifications mean, over state of the kind's own held
//! under the client's lock.
//!
//! The process has no terminal: it starts in a session of its own (through
//! [`exec_detached`]), so a passphrase or password prompt fails instead of
//! drawing over the viewer. Like a renderer, it runs in its own process
//! group, killed with everything it started when dffy exits.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, BufRead as _, BufReader, Read, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak, mpsc};
use std::time::{Duration, Instant};

use dffy_core::cache::{self, Cache};
use rustix::process::{Pid, WaitId, WaitIdOptions};

use super::json::{self, Value};
use super::{Program, kill_group, running};

/// The first argument that makes `dffy` run a plugin in its place, in a
/// session of its own (see [`exec_detached`]).
pub(crate) const DETACH: &str = "__review-plugin";

/// `dffy __review-plugin <program> <args>…`: leaves the terminal's
/// session, then becomes `program`. std starts a process in a new session
/// only from `pre_exec`, which needs unsafe code; this costs an exec, once
/// per plugin process.
pub(crate) fn exec_detached(mut args: impl Iterator<Item = OsString>) -> ExitCode {
    let _ = rustix::process::setsid();
    let Some(program) = args.next() else {
        return ExitCode::from(2);
    };
    let e = Command::new(&program).args(args).exec();
    eprintln!("can't run {}: {e}", Path::new(&program).display());
    ExitCode::from(127)
}

/// Makes a new process's command, which must put it in a process group of
/// its own.
pub(crate) type MakeCommand = Box<dyn Fn() -> io::Result<Command> + Send + Sync>;

/// Starts `program` in `root`, or where dffy runs, detached from the
/// terminal through dffy itself, with `DFFY` naming dffy.
pub(crate) fn detached(program: &Program, root: Option<&Path>) -> MakeCommand {
    let (path, root) = (program.path.clone(), root.map(Path::to_owned));
    Box::new(move || {
        let exe = std::env::current_exe()?;
        let mut command = Command::new(&exe);
        command.arg(DETACH).arg(&path).env("DFFY", exe);
        if let Some(root) = &root {
            command.current_dir(root);
        }
        Ok(command)
    })
}

/// How long `initialize` may take when only its answer is wanted. Until
/// that's cached it may run before dffy's first output, as a review
/// plugin's naming does when the arguments could name its command, so a
/// hung plugin can't hold every such start.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A process that exits this many times in a row, each within
/// [`STEADY`] of starting, isn't started again.
const MAX_CRASHES: u32 = 5;

/// A process that ran this long before exiting starts the count of
/// crashes over.
const STEADY: Duration = Duration::from_secs(60);

/// The disk cache's key for `program`'s `initialize` answer under
/// `protocol`: the program as installed, by path, modification time and
/// size.
pub(crate) fn cache_key(protocol: usize, program: &Program) -> Vec<u8> {
    let mut key = Vec::with_capacity(64);
    key.extend_from_slice(&protocol.to_le_bytes());
    key.extend_from_slice(program.path.as_os_str().as_encoded_bytes());
    key.push(0);
    key.extend_from_slice(&program.stamp.0.to_le_bytes());
    key.extend_from_slice(&program.stamp.1.to_le_bytes());
    key
}

/// The answer kept in the cache, its result as `decode` reads it: the
/// `initialize` response's line, or `{"failed":…}` with why there was
/// none; `None` for an entry that's neither.
fn cached<T>(
    line: &[u8],
    decode: impl FnOnce(&Value) -> Result<T, String>,
) -> Option<Result<T, String>> {
    let value = json::parse(line).ok()?;
    if let Some(why) = value.get("failed").and_then(Value::as_str) {
        return Some(Err(why.to_owned()));
    }
    decode(value.get("result")?).ok().map(Ok)
}

/// `program`'s `initialize` answer, its result as `decode` reads it. The
/// answer, or why it had none, is kept in the disk cache while the program
/// is the same file, so dffy doesn't start the plugin, or wait out its
/// timeout, on every run; `initialize` asks the plugin through a client
/// started with the cache, which keeps a good answer there. `fresh` asks
/// the plugin even so, and keeps what it says, as `dffy doctor` does.
pub(crate) fn describe<T>(
    (program, protocol): (&Program, usize),
    (cache, fresh): (Option<&Cache>, bool),
    decode: impl FnOnce(&Value) -> Result<T, String>,
    initialize: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let key = cache_key(protocol, program);
    let kept = (cache.filter(|_| !fresh))
        .and_then(|c| c.get(cache::Kind::Described, &key))
        .and_then(|answer| cached(&answer, decode));
    match kept {
        Some(answer) => answer.map_err(|why| format!("{why} (kept; dffy doctor asks again)")),
        None => initialize().inspect_err(|why| {
            if let Some(cache) = cache {
                let mut line = String::from("{\"failed\":");
                json::string(&mut line, why);
                line.push('}');
                cache.put(cache::Kind::Described, &key, line.as_bytes());
            }
        }),
    }
}

/// JSON-RPC's code for a method the server doesn't have.
const METHOD_NOT_FOUND: f64 = -32601.0;

/// The words for a failed request's error object; `name` is the plugin's.
pub(crate) fn error_message(name: &str, method: &str, error: &Value) -> String {
    if error.get("code") == Some(&Value::Number(METHOD_NOT_FOUND)) {
        return format!("{name} doesn't know {method}; it may be older than this dffy");
    }
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("an error without a message")
        .to_owned()
}

/// A request's id as a message carries it.
pub(crate) fn request_id(value: &Value) -> Option<u64> {
    value.as_usize().and_then(|id| u64::try_from(id).ok())
}

/// What a kind of plugin makes of what its process says: `I` what its
/// `initialize` says, `S` its state and `R` what it keeps of each request.
/// Each runs with the client's lock held, and none may call back into it:
/// on the reader thread, except `failed`, which also runs on the caller's
/// when a request is made of a client that failed or was closed, or a
/// request still waiting for `initialize` is cancelled.
pub(crate) struct Kind<I, S, R> {
    /// Names the client's threads, then `-in` and `-err`.
    pub thread: &'static str,
    /// The protocol version, sent in `initialize`.
    pub protocol: usize,
    /// Fields of the kind's own for `initialize`'s params, from its state,
    /// each after a comma: `,"agent":"codex"`, or nothing.
    pub init_params: fn(&S) -> String,
    /// How long `close` waits for the process to exit after `exit` before
    /// killing its group: a plugin that starts processes in groups of
    /// their own must stop them itself, as killing its group misses them.
    pub grace: Duration,
    /// Reads `initialize`'s result, or refuses it. What it sends goes
    /// before the requests that waited.
    pub initialized: Initialized<I, S, R>,
    /// A response to a request, no longer pending; dropping the guard lets
    /// other requests go on while it's read.
    pub answered: Answered<I, S, R>,
    /// A notification, by method and params.
    pub notified: Notified<I, S, R>,
    /// Ends a request the plugin won't answer, saying why.
    pub failed: fn(Pending<R>, String),
}

type Initialized<I, S, R> = fn(&mut Inner<I, S, R>, &Value) -> Result<I, String>;
type Answered<I, S, R> = fn(&Shared<I, S, R>, Guard<'_, I, S, R>, Pending<R>, &Value);
type Notified<I, S, R> = fn(&Shared<I, S, R>, Guard<'_, I, S, R>, &str, &Value);
pub(crate) type Guard<'a, I, S, R> = MutexGuard<'a, Inner<I, S, R>>;

/// A running plugin: one process, started again when it crashes, that
/// answers requests. Requests made before it has initialized wait for it.
/// Cloning shares the process.
pub(crate) struct Client<I, S, R>(Arc<Shared<I, S, R>>);

impl<I, S, R> Clone for Client<I, S, R> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub(crate) struct Shared<I, S, R> {
    /// The program's name, as `dffy-review-github`, for messages.
    name: String,
    command: MakeCommand,
    /// Where its `initialize` answer is kept, under which key.
    cache: Option<(Cache, Vec<u8>)>,
    kind: Kind<I, S, R>,
    state: Mutex<Inner<I, S, R>>,
}

pub(crate) struct Inner<I, S, R> {
    phase: Phase,
    /// The process now; `None` while one restarts.
    process: Option<Process>,
    next_id: u64,
    pub pending: HashMap<u64, Pending<R>>,
    /// Requests made before the process initialized, by id, in order.
    queued: Vec<(u64, Line)>,
    /// Exits in a row, each soon after its start.
    crashes: u32,
    /// What the process that initialized last said.
    pub init: Option<I>,
    on_ready: Option<OnReady<I>>,
    /// The kind's own.
    pub kind: S,
}

type OnReady<I> = Box<dyn FnOnce(Result<&I, String>) + Send>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Starting,
    Ready,
    /// It won't run again: requests fail with this.
    Failed(String),
    Closed,
}

struct Process {
    leader: Pid,
    /// Lines for its stdin, written by a thread of its own.
    input: mpsc::Sender<Line>,
    /// Disconnects once the process exited and was reaped.
    gone: mpsc::Receiver<()>,
    /// Its `initialize` request's id.
    init: u64,
    started: Instant,
}

/// A request sent, or waiting for the process to initialize.
pub(crate) struct Pending<R> {
    /// dffy ended it before the plugin answered, and asked the plugin to
    /// stop: nothing more of it is delivered, though the plugin may still
    /// send for it until it answers.
    pub ended: bool,
    pub request: R,
}

/// A request made, to cancel.
pub(crate) struct Run<I, S, R> {
    client: Weak<Shared<I, S, R>>,
    id: u64,
}

impl<I, S, R> std::fmt::Debug for Run<I, S, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl<I: Send + 'static, S: Send + 'static, R: Send + 'static> Run<I, S, R> {
    /// Asks the plugin to stop the request; it still ends as the kind says
    /// it does.
    pub(crate) fn cancel(&self) {
        if let Some(shared) = self.client.upgrade() {
            Client(shared).cancel(self.id);
        }
    }
}

impl<I: Send + 'static, S: Send + 'static, R: Send + 'static> Client<I, S, R> {
    /// Starts the process `command` makes, as `name`, with `kind` reading
    /// what it says into `state`. A good `initialize` answer is kept in
    /// `cache`, under its key.
    pub(crate) fn spawn(
        name: String,
        command: MakeCommand,
        cache: Option<(Cache, Vec<u8>)>,
        kind: Kind<I, S, R>,
        state: S,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            name,
            command,
            cache,
            kind,
            state: Mutex::new(Inner {
                phase: Phase::Starting,
                process: None,
                next_id: 0,
                pending: HashMap::new(),
                queued: Vec::new(),
                crashes: 0,
                init: None,
                on_ready: None,
                kind: state,
            }),
        });
        launch(&shared)?;
        Ok(Self(shared))
    }

    pub(crate) fn lock(&self) -> Guard<'_, I, S, R> {
        self.0.lock()
    }

    /// Hands what `initialize` said to `ready` once the plugin has
    /// initialized, or why it can't: now, if it has.
    pub(crate) fn on_ready(&self, ready: impl FnOnce(Result<&I, String>) + Send + 'static) {
        let mut inner = self.0.lock();
        match (&inner.phase, &inner.init) {
            (Phase::Failed(e), _) => ready(Err(e.clone())),
            (Phase::Closed, _) => {}
            (_, Some(init)) => ready(Ok(init)),
            (_, None) => inner.on_ready = Some(Box::new(ready)),
        }
    }

    /// Waits up to [`DESCRIBE_TIMEOUT`] for the plugin to initialize, then
    /// closes it: what `read` takes of what it said, or why there's none.
    pub(crate) fn wait_initialized<T: Send + 'static>(
        self,
        read: impl FnOnce(&I) -> T + Send + 'static,
    ) -> Result<T, String> {
        let (tx, rx) = mpsc::channel();
        self.on_ready(move |ready| {
            tx.send(ready.map(read)).ok();
        });
        let answer = rx.recv_timeout(DESCRIBE_TIMEOUT);
        self.close();
        answer.unwrap_or_else(|_| {
            Err(format!(
                "its initialize took over {}s",
                DESCRIBE_TIMEOUT.as_secs()
            ))
        })
    }

    /// Sends `method`, its params made by `params` on the thread that
    /// writes them, which has the client and the request's id; `None`
    /// sends nothing, and `params` ends the request itself then. A plugin
    /// that has failed or was closed ends it at once, through the kind.
    pub(crate) fn request(
        &self,
        method: &'static str,
        request: R,
        params: impl FnOnce(&Shared<I, S, R>, u64) -> Option<String> + Send + 'static,
    ) -> Run<I, S, R> {
        let mut inner = self.0.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        let run = Run {
            client: Arc::downgrade(&self.0),
            id,
        };
        let shared = Arc::downgrade(&self.0);
        let line: Line = Box::new(move || {
            let shared = shared.upgrade()?;
            params(&shared, id).map(|params| message(Some(id), method, &params))
        });
        let pending = Pending {
            ended: false,
            request,
        };
        match &inner.phase {
            Phase::Failed(e) => {
                (self.0.kind.failed)(pending, e.clone());
                return run;
            }
            Phase::Closed => {
                (self.0.kind.failed)(pending, format!("{} was stopped", self.0.name));
                return run;
            }
            Phase::Ready => inner.send(line),
            Phase::Starting => inner.queued.push((id, line)),
        }
        inner.pending.insert(id, pending);
        run
    }

    fn cancel(&self, id: u64) {
        let mut inner = self.0.lock();
        if inner.pending.get(&id).is_none_or(|p| p.ended) {
            return;
        }
        if let Some(at) = inner.queued.iter().position(|(q, _)| *q == id) {
            drop(inner.queued.remove(at));
            if let Some(pending) = inner.pending.remove(&id) {
                (self.0.kind.failed)(pending, "cancelled".to_owned());
            }
            return;
        }
        inner.send_cancel(id);
    }

    /// Asks the plugin to shut down and exit, waits up to the kind's
    /// [`Kind::grace`] for it to, then kills it with whatever it started:
    /// nothing it does from here on is waited for.
    pub(crate) fn close(&self) {
        let Some(leader) = self.0.shut_down(self.0.lock()) else {
            return;
        };
        let running = running();
        // Once reaped, its id may be another process's.
        if running.groups.contains(&leader) {
            kill_group(leader);
            // Before the detached process leaves dffy's group, it has none.
            rustix::process::kill_process(leader, rustix::process::Signal::KILL).ok();
        }
    }
}

impl<I: Send + Sync + 'static, S: Send + 'static, R: Send + 'static> Client<I, S, R> {
    /// Has a panic ask the plugin to shut down, and give it the kind's
    /// [`Kind::grace`] to, before the panic hook kills it
    /// ([`super::stop_on_panic`]): what it started in process groups of
    /// its own, which that kill doesn't reach, it stops itself.
    pub(crate) fn shut_down_on_panic(&self) {
        let shared = Arc::downgrade(&self.0);
        super::on_panic(move || {
            let Some(shared) = shared.upgrade() else {
                return;
            };
            // The panicking thread may hold it, as in a request's callback.
            let inner = match shared.state.try_lock() {
                Ok(inner) => inner,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return,
            };
            shared.shut_down(inner);
        });
    }
}

impl<I, S, R> Shared<I, S, R> {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Closes the plugin and asks its process to shut down and exit, then
    /// waits up to the kind's [`Kind::grace`] for it to. Returns its group
    /// leader, for the kill after; `None` when no process runs.
    fn shut_down(&self, mut inner: Guard<'_, I, S, R>) -> Option<Pid> {
        inner.phase = Phase::Closed;
        inner.queued.clear();
        let Process {
            leader,
            input,
            gone,
            ..
        } = inner.process.take()?;
        let id = inner.next_id + 1;
        inner.next_id = id;
        input
            .send(text_line(message(Some(id), "shutdown", "{}")))
            .ok();
        input.send(text_line(message(None, "exit", "{}"))).ok();
        // Unlocked: the reader takes the lock for what the process says
        // before it exits.
        drop(inner);
        // Its stdin closes once what was sent is written.
        drop(input);
        if !self.kind.grace.is_zero() {
            gone.recv_timeout(self.kind.grace).ok();
        }
        Some(leader)
    }

    pub(crate) fn lock(&self) -> Guard<'_, I, S, R> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A message for the plugin's stdin, made on the thread that writes it, so
/// a request whose params read files reads them there; `None` sends
/// nothing.
type Line = Box<dyn FnOnce() -> Option<String> + Send>;

fn text_line(text: String) -> Line {
    Box::new(move || Some(text))
}

impl<I, S, R> Inner<I, S, R> {
    /// Initialized, and not since exited.
    pub(crate) fn ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    fn send(&self, line: Line) {
        if let Some(process) = &self.process {
            process.input.send(line).ok();
        }
    }

    /// Sends the notification `method` with `params`, a JSON object.
    pub(crate) fn notify(&self, method: &str, params: &str) {
        self.send(text_line(message(None, method, params)));
    }

    /// Asks the plugin to stop request `id`.
    pub(crate) fn send_cancel(&self, id: u64) {
        self.notify("$/cancelRequest", &format!("{{\"id\":{id}}}"));
    }

    /// Ends every request with `why`, through `failed`.
    fn fail_all(&mut self, failed: fn(Pending<R>, String), why: &str) {
        self.queued.clear();
        for (_, pending) in self.pending.drain() {
            failed(pending, why.to_owned());
        }
    }

    /// Ends request `id` here, before the plugin answers it: the plugin is
    /// asked to stop, and what it sends until it answers still reaches the
    /// kind.
    pub(crate) fn end(&mut self, id: u64) {
        if let Some(pending) = self.pending.get_mut(&id).filter(|p| !p.ended) {
            pending.ended = true;
            self.send_cancel(id);
        }
    }
}

/// A JSON-RPC message: a request with `id`, else a notification.
fn message(id: Option<u64>, method: &str, params: &str) -> String {
    let mut out = String::from("{\"jsonrpc\":\"2.0\",");
    if let Some(id) = id {
        let _ = write!(out, "\"id\":{id},");
    }
    out.push_str("\"method\":");
    json::string(&mut out, method);
    let _ = write!(out, ",\"params\":{params}}}");
    out
}

/// Starts a process for `shared` and sends it `initialize`; its reader
/// takes it from there.
fn launch<I: Send + 'static, S: Send + 'static, R: Send + 'static>(
    shared: &Arc<Shared<I, S, R>>,
) -> io::Result<()> {
    let mut child = (shared.command)()?
        .stdin(Stdio::piped())
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
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        super::kill(&mut child);
        return Err(io::Error::other("no pipes"));
    };
    let (input, lines) = mpsc::channel::<Line>();
    let thread = shared.kind.thread;
    // A plugin that stops reading blocks this thread, never the caller; nor
    // does making a request's params.
    let writer = std::thread::Builder::new()
        .name(format!("{thread}-in"))
        .spawn(move || {
            for line in lines {
                let Some(mut line) = line() else { continue };
                line.push('\n');
                if stdin.write_all(line.as_bytes()).is_err() {
                    return;
                }
            }
        });
    let last_said = writer.and_then(|_| {
        std::thread::Builder::new()
            .name(format!("{thread}-err"))
            .spawn(move || last_line(stderr))
    });
    let last_said = match last_said {
        Ok(thread) => thread,
        Err(e) => {
            super::kill(&mut child);
            return Err(e);
        }
    };
    let mut inner = shared.lock();
    // Closed while this one was starting, as after a crash: it's not kept.
    if inner.phase == Phase::Closed {
        drop(inner);
        super::kill(&mut child);
        return Err(io::Error::other(format!("{} was stopped", shared.name)));
    }
    inner.next_id += 1;
    let init = inner.next_id;
    let mut params = format!(
        "{{\"protocol\":{},\"client\":{{\"name\":\"dffy\",\"version\":",
        shared.kind.protocol
    );
    json::string(&mut params, env!("CARGO_PKG_VERSION"));
    params.push('}');
    params.push_str(&(shared.kind.init_params)(&inner.kind));
    params.push('}');
    input
        .send(text_line(message(Some(init), "initialize", &params)))
        .ok();
    let (reaped, gone) = mpsc::channel();
    inner.process = Some(Process {
        leader,
        input,
        gone,
        init,
        started: Instant::now(),
    });
    inner.phase = Phase::Starting;
    drop(inner);
    let reading = Arc::clone(shared);
    let reader = std::thread::Builder::new()
        .name(thread.into())
        .spawn(move || read(&reading, stdout, (child, leader, reaped), last_said));
    if let Err(e) = reader {
        kill_group(leader);
        return Err(e);
    }
    Ok(())
}

/// Reads the process's messages until it exits, then ends what it left
/// unanswered and starts another, unless it shouldn't.
fn read<I: Send + 'static, S: Send + 'static, R: Send + 'static>(
    shared: &Arc<Shared<I, S, R>>,
    stdout: impl Read,
    (mut child, leader, reaped): (Child, Pid, mpsc::Sender<()>),
    last_said: std::thread::JoinHandle<Option<String>>,
) {
    let mut failure = None;
    for line in BufReader::new(stdout).split(b'\n') {
        let Ok(line) = line else { break };
        if line.trim_ascii().is_empty() {
            continue;
        }
        if let Err(e) = handle(shared, &line) {
            let why = format!("{}: {e}", shared.name);
            let mut inner = shared.lock();
            // Before it initialized, it's refused rather than crashed.
            if inner.phase == Phase::Starting {
                inner.phase = Phase::Failed(why.clone());
                if let Some(ready) = inner.on_ready.take() {
                    ready(Err(why.clone()));
                }
            }
            drop(inner);
            failure = Some(why);
            kill_group(leader);
            break;
        }
    }
    let status = reap(&mut child, leader);
    drop(reaped);
    let stderr = last_said.join().ok().flatten();
    let why = failure.unwrap_or_else(|| {
        let said = stderr.map(|s| format!(": {s}")).unwrap_or_default();
        let how = status.map_or_else(|| "was killed".to_owned(), super::exited);
        format!("{} {how}{said}", shared.name)
    });
    exited(shared, &why);
}

/// Takes one message from the plugin. An error is one dffy can't trust,
/// which stops the process.
fn handle<I, S, R>(shared: &Shared<I, S, R>, line: &[u8]) -> Result<(), String> {
    let value = json::parse(line)?;
    if !matches!(value, Value::Object(_)) {
        return Err("a message isn't a JSON object".to_owned());
    }
    let mut inner = shared.lock();
    match (value.get("id"), value.get("method").and_then(Value::as_str)) {
        // A request of the plugin's, which dffy answers none of, or neither.
        (Some(_), Some(_)) | (None, None) => {}
        (Some(id), None) => {
            let Some(id) = request_id(id) else {
                return Ok(());
            };
            if inner.process.as_ref().is_some_and(|p| p.init == id) {
                return initialized(shared, &mut inner, &value);
            }
            let Some(pending) = inner.pending.remove(&id) else {
                return Ok(());
            };
            (shared.kind.answered)(shared, inner, pending, &value);
        }
        (None, Some(method)) => {
            let params = value.get("params").unwrap_or(&Value::Null);
            (shared.kind.notified)(shared, inner, method, params);
        }
    }
    Ok(())
}

/// The answer to `initialize`: the process is ready for what waited, or
/// refused, and isn't started again.
fn initialized<I, S, R>(
    shared: &Shared<I, S, R>,
    inner: &mut Inner<I, S, R>,
    value: &Value,
) -> Result<(), String> {
    let init = match (value.get("result"), value.get("error")) {
        (_, Some(error)) if *error != Value::Null => {
            Err(error_message(&shared.name, "initialize", error))
        }
        (Some(result), _) => (shared.kind.initialized)(inner, result),
        (None, _) => Err("its initialize answered nothing".to_owned()),
    };
    match init {
        Ok(init) => {
            if let Some((cache, key)) = &shared.cache {
                cache.put(cache::Kind::Described, key, value_line(value).as_bytes());
            }
            inner.phase = Phase::Ready;
            for (_, line) in std::mem::take(&mut inner.queued) {
                inner.send(line);
            }
            let init = &*inner.init.insert(init);
            if let Some(ready) = inner.on_ready.take() {
                ready(Ok(init));
            }
            Ok(())
        }
        Err(e) => {
            inner.phase = Phase::Failed(e.clone());
            inner.fail_all(shared.kind.failed, &e);
            if let Some(ready) = inner.on_ready.take() {
                ready(Err(e.clone()));
            }
            Err(e)
        }
    }
}

/// The `initialize` response as the cache keeps it: its result alone, in
/// a response's shape.
fn value_line(value: &Value) -> String {
    let mut out = String::from("{\"result\":");
    json::write(&mut out, value.get("result").unwrap_or(&Value::Null));
    out.push('}');
    out
}

/// The process is gone: what it left unanswered ends with `why`, and
/// another starts after a pause that doubles with each crash in a row,
/// unless dffy is exiting, the plugin was closed or refused, or it keeps
/// crashing.
fn exited<I: Send + 'static, S: Send + 'static, R: Send + 'static>(
    shared: &Arc<Shared<I, S, R>>,
    why: &str,
) {
    let mut inner = shared.lock();
    let uptime = inner.process.take().map(|p| p.started.elapsed());
    inner.fail_all(shared.kind.failed, why);
    if !matches!(inner.phase, Phase::Starting | Phase::Ready) || running().stopped {
        return;
    }
    inner.crashes = if uptime.is_some_and(|up| up >= STEADY) {
        1
    } else {
        inner.crashes + 1
    };
    if inner.crashes > MAX_CRASHES {
        let e = format!("{why}; not started again after {MAX_CRASHES} exits in a row");
        inner.phase = Phase::Failed(e.clone());
        if let Some(ready) = inner.on_ready.take() {
            ready(Err(e));
        }
        return;
    }
    inner.phase = Phase::Starting;
    let pause = Duration::from_secs(1 << (inner.crashes - 1));
    drop(inner);
    std::thread::sleep(pause);
    if shared.lock().phase != Phase::Starting {
        return;
    }
    if let Err(e) = launch(shared) {
        let mut inner = shared.lock();
        let e = format!("couldn't start {} again: {e}", shared.name);
        inner.fail_all(shared.kind.failed, &e);
        if inner.phase == Phase::Starting {
            inner.phase = Phase::Failed(e);
        }
    }
}

/// Waits for the process to exit, then kills what it left in its group and
/// reaps it in one step under the list's lock, so [`super::stop_all`]
/// never signals a reused id.
fn reap(child: &mut Child, leader: Pid) -> Option<ExitStatus> {
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
