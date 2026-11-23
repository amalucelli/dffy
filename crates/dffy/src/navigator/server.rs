//! `dffy __navigate`: the helper process that owns a repository's
//! navigators for one host, viewer or MCP server, and answers its lookups
//! and searches over stdin and stdout (docs/navigate.md). It holds the
//! index of definitions, which an allocator keeps once it's freed, so the
//! memory goes back when the process exits: when idle, or when its host
//! goes away.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, BufRead as _, Write as _};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dffy_core::cache::Cache;
use dffy_core::diff::Options;
use dffy_core::git::{Repo, Snapshot};
use dffy_core::navigate::{
    self, Fit, MAX_REFERENCE_FILES, Navigator, Origin, References, TextMatches,
};
use dffy_core::view::Side;

use super::wire::{self, field, number, text};
use super::{IDLE, NAVIGATORS, PROTOCOL, SEARCHES};
use crate::plugin::json::{self, Value, string};

/// JSON-RPC's codes, and the protocol's own.
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const INVALID_PARAMS: i32 = -32602;
const METHOD_NOT_FOUND: i32 = -32601;
const FAILED: i32 = -32000;
const GONE: i32 = -32001;
const NOT_INITIALIZED: i32 = -32002;
const CANCELLED: i32 = -32800;

/// The methods `initialize` says the helper has.
const METHODS: &[&str] = &[
    "open",
    "lookup",
    "progress",
    "search/start",
    "search/step",
    "search/close",
];

/// Serves the protocol on stdin and stdout until stdin closes, `exit`
/// comes, or no request came for the idle time.
pub(crate) fn serve() -> ExitCode {
    // A process group of its own, which the host kills with what it
    // started; a terminal's signals don't reach it.
    let _ = rustix::process::setsid();
    let (activity, idle) = mpsc::channel();
    let (out, busy) = (
        Arc::new(Mutex::new(io::stdout())),
        Arc::new(AtomicUsize::new(0)),
    );
    let server = Arc::new(Server {
        out: Arc::clone(&out),
        state: Mutex::new(State::new(out, Arc::clone(&busy))),
        cancels: Mutex::new(HashMap::new()),
        busy,
        activity: Mutex::new(activity),
    });
    let waiting = Arc::clone(&server);
    // One that can't start leaves the helper to its host to stop.
    drop(
        std::thread::Builder::new()
            .name("navigate-idle".into())
            .spawn(move || exit_when_idle(&waiting, &idle)),
    );
    let stdin = io::stdin().lock();
    for line in stdin.split(b'\n') {
        let Ok(line) = line else { break };
        if line.trim_ascii().is_empty() {
            continue;
        }
        server.message(&line);
    }
    ExitCode::SUCCESS
}

/// Exits once no request came for the idle time, `initialize`'s or
/// [`IDLE`], and no request or index build runs.
fn exit_when_idle(server: &Server, activity: &mpsc::Receiver<Duration>) {
    let mut idle = IDLE;
    loop {
        match activity.recv_timeout(idle) {
            Ok(set) => {
                if !set.is_zero() {
                    idle = set;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if server.busy.load(Ordering::Relaxed) == 0 {
                    leave();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Ends the process from any thread, the index's threads with it: what the
/// helper holds is all in memory, rebuilt from the disk cache next time.
#[expect(
    clippy::exit,
    reason = "the idle timer and `exit` end the process from outside the reading loop"
)]
fn leave() -> ! {
    std::process::exit(0)
}

/// Writes `line` to the host, a message a line.
fn write_line(out: &Mutex<io::Stdout>, line: &str) {
    let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
    // A host gone away closes stdin too, which ends the helper.
    (out.write_all(line.as_bytes()))
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
        .ok();
}

/// One more request or build running, until dropped.
struct Busy(Arc<AtomicUsize>);

impl Busy {
    fn new(busy: &Arc<AtomicUsize>) -> Self {
        busy.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(busy))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// How often a build pushes how far it is, at most: 4 times a second.
const PUSH_EVERY: Duration = Duration::from_millis(250);

/// The members of how far `navigator`'s index is: as `progress` answers,
/// and the helper pushes it.
fn built(navigator: &Navigator, building: bool, error: Option<&str>) -> String {
    let mut s = String::from(r#""progress":"#);
    wire::progress(navigator.progress(), &mut s);
    let _ = write!(s, r#","building":{building},"error":"#);
    match error {
        Some(e) => string(&mut s, e),
        None => s.push_str("null"),
    }
    let stats = navigator.stats();
    let _ = write!(
        s,
        r#","skipped":{{"too_large":{},"timed_out":{},"unreadable":{}}}"#,
        stats.too_large, stats.timed_out, stats.unreadable
    );
    s
}

/// Tells the host how far `navigator`'s index is.
fn push(out: &Mutex<io::Stdout>, navigator: &Navigator, building: bool, error: Option<&str>) {
    let line = format!(
        r#"{{"jsonrpc":"2.0","method":"progress","params":{{"navigator":{},{}}}}}"#,
        navigator.id(),
        built(navigator, building, error)
    );
    write_line(out, &line);
}

struct Server {
    out: Arc<Mutex<io::Stdout>>,
    state: Mutex<State>,
    /// The flags that stop requests running, by id.
    cancels: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    /// Requests and builds running.
    busy: Arc<AtomicUsize>,
    /// Each request pings the idle timer; `initialize` sets its time.
    activity: Mutex<mpsc::Sender<Duration>>,
}

struct State {
    /// What `initialize` set up.
    setup: Option<Setup>,
    shared: navigate::Shared,
    /// At most [`NAVIGATORS`], least recently used first.
    navigators: Vec<Held>,
    /// Builds stopped, still winding down: the next build waits for them.
    stopping: Vec<JoinHandle<()>>,
    /// Most recent last, at most [`SEARCHES`].
    /// By id, with their navigator's.
    searches: Vec<(u64, u64, Arc<Mutex<Search>>)>,
    next_search: u64,
    /// Where builds push how far they are, and the count of requests and
    /// builds running they join.
    out: Arc<Mutex<io::Stdout>>,
    busy: Arc<AtomicUsize>,
}

struct Setup {
    repo: Repo,
    cache: Cache,
    threads: usize,
}

/// A navigator of one side of the diff between `snapshots`.
struct Held {
    snapshots: [Snapshot; 2],
    side: Side,
    navigator: Arc<Navigator>,
    /// The one it replaced, until its index is built: it holds the
    /// definitions read so far, which the build takes from the shared
    /// store.
    previous: Option<Arc<Navigator>>,
    /// Its index build while one may run: the flag that stops it, and its
    /// thread.
    build: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    /// A tags query that doesn't compile, as the build found.
    error: Arc<Mutex<Option<String>>>,
}

enum Search {
    References {
        navigator: Arc<Navigator>,
        search: References,
        found: usize,
    },
    Text {
        navigator: Arc<Navigator>,
        search: TextMatches,
        found: usize,
    },
}

/// A request's failure: its code and message.
type Failure = (i32, String);

fn invalid(e: String) -> Failure {
    (INVALID_PARAMS, e)
}

impl Held {
    fn stop_build(&mut self, stopping: &mut Vec<JoinHandle<()>>) {
        if let Some((cancel, thread)) = self.build.take() {
            cancel.store(true, Ordering::Relaxed);
            stopping.push(thread);
        }
    }
}

impl State {
    fn new(out: Arc<Mutex<io::Stdout>>, busy: Arc<AtomicUsize>) -> Self {
        Self {
            setup: None,
            shared: navigate::Shared::default(),
            navigators: Vec::new(),
            stopping: Vec::new(),
            searches: Vec::new(),
            next_search: 0,
            out,
            busy,
        }
    }

    /// The navigator `id`, now the one used last, whose index builds.
    fn touch(&mut self, id: u64) -> Result<Arc<Navigator>, Failure> {
        let at = (self.navigators.iter())
            .position(|h| h.navigator.id() == id)
            .ok_or_else(|| (GONE, format!("no navigator {id}; open it again")))?;
        let held = self.navigators.remove(at);
        let navigator = Arc::clone(&held.navigator);
        self.navigators.push(held);
        self.build();
        Ok(navigator)
    }

    /// Builds the index of the navigator used last, the one build that
    /// runs: any other stops, to go on once its navigator is used again.
    fn build(&mut self) {
        let Self {
            setup,
            navigators,
            stopping,
            out,
            busy,
            ..
        } = self;
        let Some(setup) = setup else { return };
        stopping.retain(|thread| !thread.is_finished());
        let Some((_, others)) = navigators.split_last_mut() else {
            return;
        };
        for other in others {
            other.stop_build(stopping);
        }
        for held in navigators.iter_mut() {
            if held.navigator.progress().complete() {
                held.previous = None;
            }
        }
        let Some(last) = navigators.last_mut() else {
            return;
        };
        let running = (last.build.as_ref()).is_some_and(|(_, thread)| !thread.is_finished());
        if running || last.navigator.progress().complete() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let waits = std::mem::take(stopping);
        let (navigator, stop, error) = (
            Arc::clone(&last.navigator),
            Arc::clone(&cancel),
            Arc::clone(&last.error),
        );
        let (repo, cache, threads) = (
            setup.repo.without_object_cache(),
            setup.cache.symbols(),
            setup.threads,
        );
        let (out, running) = (Arc::clone(out), Busy::new(busy));
        let spawned = std::thread::Builder::new()
            .name("navigate-index".into())
            .spawn(move || {
                let _running = running;
                for wait in waits {
                    drop(wait.join());
                }
                push(&out, &navigator, true, None);
                let mut pushed = Instant::now();
                let built = navigator.build(&repo, &cache, threads, &stop, |progress| {
                    if pushed.elapsed() >= PUSH_EVERY && !progress.complete() {
                        pushed = Instant::now();
                        push(&out, &navigator, true, None);
                    }
                });
                let mut error = error.lock().unwrap_or_else(PoisonError::into_inner);
                if let Err(e) = built {
                    *error = Some(e.to_string());
                }
                // Done, or stopped for another's build: it isn't building.
                push(&out, &navigator, false, error.as_deref());
            });
        // One that can't start leaves lookups to the files near the asking
        // one, and is tried again on the next request.
        last.build = spawned.ok().map(|thread| (cancel, thread));
    }

    /// Holds `navigator`, of `side` of the diff between `snapshots`, as the
    /// one used last: the one it replaces stays as its `previous`, the
    /// least recently used past [`NAVIGATORS`] goes, with its searches.
    fn hold(&mut self, snapshots: [Snapshot; 2], side: Side, navigator: Arc<Navigator>) {
        let Self {
            navigators,
            stopping,
            searches,
            ..
        } = self;
        let mut previous = None;
        if let Some(at) =
            (navigators.iter()).position(|h| h.snapshots == snapshots && h.side == side)
        {
            let mut old = navigators.remove(at);
            old.stop_build(stopping);
            previous = Some(old.navigator);
        }
        while navigators.len() >= NAVIGATORS {
            let mut gone = navigators.remove(0);
            gone.stop_build(stopping);
        }
        navigators.push(Held {
            snapshots,
            side,
            navigator,
            previous,
            build: None,
            error: Arc::new(Mutex::new(None)),
        });
        searches.retain(|(_, of, _)| navigators.iter().any(|h| h.navigator.id() == *of));
        self.build();
    }

    fn setup(&self) -> Result<&Setup, Failure> {
        (self.setup.as_ref()).ok_or_else(|| (NOT_INITIALIZED, "initialize first".to_owned()))
    }
}

impl Server {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn message(self: &Arc<Self>, line: &[u8]) {
        let Some(value) = json::parse(line)
            .ok()
            .filter(|v| matches!(v, Value::Object(_)))
        else {
            // Not a message whose id could be read: JSON-RPC answers with a
            // null id.
            self.write_answer(None, Err((PARSE_ERROR, "not a JSON object".to_owned())));
            return;
        };
        let method = value.get("method").and_then(Value::as_str);
        let id = value
            .get("id")
            .and_then(Value::as_usize)
            .map(|id| id as u64);
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some("exit"), None) => leave(),
            (Some("$/cancelRequest"), None) => {
                if let Some(id) = params.get("id").and_then(Value::as_usize) {
                    let cancels = self.cancels.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(flag) = cancels.get(&(id as u64)) {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            }
            (Some("search/close"), None) => {
                if let Some(search) = params.get("search").and_then(Value::as_usize) {
                    self.state().searches.retain(|(s, ..)| *s != search as u64);
                }
            }
            (Some(method), Some(id)) => self.spawn_request(method, id, params),
            // A response, which the helper asked for none of.
            (None, Some(_)) if value.get("result").is_some() || value.get("error").is_some() => {}
            (None, Some(id)) => {
                self.write_answer(Some(id), Err((INVALID_REQUEST, "no method".to_owned())));
            }
            // Another notification, which the helper has none of.
            _ => {}
        }
    }

    /// Runs request `id` on a thread of its own, so a lookup needn't wait
    /// for a search's step, and a cancel is read meanwhile. A request that
    /// panics answers as failed.
    fn spawn_request(self: &Arc<Self>, method: &str, id: u64, params: Value) {
        let (method, server) = (method.to_owned(), Arc::clone(self));
        let cancel = Arc::new(AtomicBool::new(false));
        (self.cancels.lock().unwrap_or_else(PoisonError::into_inner))
            .insert(id, Arc::clone(&cancel));
        let running = Busy::new(&self.busy);
        let ping = (self.activity.lock().unwrap_or_else(PoisonError::into_inner)).clone();
        ping.send(Duration::ZERO).ok();
        let spawned = std::thread::Builder::new()
            .name("navigate-request".into())
            .spawn(move || {
                let _running = running;
                let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    server.request(&method, &params, &cancel)
                }))
                .unwrap_or_else(|_| Err((FAILED, format!("the helper failed on {method}"))));
                server.answer(id, answer);
            });
        if spawned.is_err() {
            self.answer(id, Err((FAILED, "couldn't start a thread".to_owned())));
        }
    }

    fn answer(&self, id: u64, answer: Result<String, Failure>) {
        (self.cancels.lock().unwrap_or_else(PoisonError::into_inner)).remove(&id);
        self.write_answer(Some(id), answer);
    }

    fn write_answer(&self, id: Option<u64>, answer: Result<String, Failure>) {
        let mut line = String::from(r#"{"jsonrpc":"2.0","id":"#);
        match id {
            Some(id) => {
                let _ = write!(line, "{id},");
            }
            None => line.push_str("null,"),
        }
        match answer {
            Ok(result) => {
                let _ = write!(line, r#""result":{result}}}"#);
            }
            Err((code, message)) => {
                let _ = write!(line, r#""error":{{"code":{code},"message":"#);
                string(&mut line, &message);
                line.push_str("}}");
            }
        }
        write_line(&self.out, &line);
    }

    fn request(
        &self,
        method: &str,
        params: &Value,
        cancel: &AtomicBool,
    ) -> Result<String, Failure> {
        if !matches!(method, "initialize" | "shutdown") && METHODS.contains(&method) {
            self.state().setup()?;
        }
        match method {
            "initialize" => self.initialize(params),
            "shutdown" => Ok("null".to_owned()),
            "open" => self.open(params),
            "lookup" => self.lookup(params, cancel),
            "progress" => self.progress(params),
            "search/start" => self.start(params, cancel),
            "search/step" => self.step(params, cancel),
            _ => Err((METHOD_NOT_FOUND, format!("no method {method}"))),
        }
    }

    /// `{"protocol","client","threads","cache":{"dir","bytes"},"idle_ms"?}`:
    /// the repository is the one the helper runs in.
    fn initialize(&self, params: &Value) -> Result<String, Failure> {
        let protocol = number(params, "protocol").map_err(invalid)?;
        if protocol != PROTOCOL {
            return Err((
                FAILED,
                format!("this dffy speaks navigator protocol {PROTOCOL}, not {protocol}"),
            ));
        }
        let threads = number(params, "threads").map_err(invalid)?.clamp(1, 16);
        let cache = field(params, "cache").map_err(invalid)?;
        let cache = Cache::new(
            text(cache, "dir").map_err(invalid)?.into(),
            number(cache, "bytes").map_err(invalid)? as u64,
        );
        let cwd = std::env::current_dir().map_err(|e| (FAILED, e.to_string()))?;
        let repo = Repo::discover(&cwd).map_err(|e| (FAILED, e.to_string()))?;
        if let Some(ms) = params.get("idle_ms").and_then(Value::as_usize) {
            let ping = (self.activity.lock().unwrap_or_else(PoisonError::into_inner)).clone();
            ping.send(Duration::from_millis(ms as u64).max(Duration::from_millis(1)))
                .ok();
        }
        self.state().setup = Some(Setup {
            repo,
            cache,
            threads,
        });
        let mut s = format!(r#"{{"protocol":{PROTOCOL},"capabilities":{{"methods":["#);
        for (i, method) in METHODS.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            string(&mut s, method);
        }
        s.push_str("]}}");
        Ok(s)
    }

    /// `{"snapshots":[…,…],"side","whitespace"}`: the navigator of that
    /// side, kept while the core says it fits the diff as it is now, else
    /// made again; either way the one whose index builds.
    fn open(&self, params: &Value) -> Result<String, Failure> {
        let both = field(params, "snapshots")
            .map_err(invalid)?
            .as_array()
            .filter(|s| s.len() == 2)
            .ok_or_else(|| invalid("snapshots isn't two snapshots".to_owned()))?;
        let snapshots = [
            wire::read_snapshot(&both[0]).map_err(invalid)?,
            wire::read_snapshot(&both[1]).map_err(invalid)?,
        ];
        let side = wire::read_side(params, "side").map_err(invalid)?;
        let whitespace = wire::read_whitespace(params, "whitespace").map_err(invalid)?;
        let (repo, shared) = {
            let state = self.state();
            (state.setup()?.repo.clone(), state.shared.clone())
        };
        let changes =
            navigate::changes_between(&repo, snapshots).map_err(|e| (FAILED, e.to_string()))?;
        let diff = navigate::Diff {
            snapshots,
            changes: &changes,
            options: Options {
                whitespace,
                ..Options::default()
            },
        };
        let kept = {
            let state = self.state();
            (state.navigators.iter())
                .find(|h| h.snapshots == snapshots && h.side == side)
                .map(|h| Arc::clone(&h.navigator))
        };
        let fit = kept.as_ref().map_or(Fit::New, |n| n.refit(&repo, &diff));
        let navigator = if let (Some(navigator), Fit::Same | Fit::Changes) = (kept, fit) {
            self.state().touch(navigator.id())?;
            navigator
        } else {
            // Listed unlocked: other requests go on meanwhile.
            let navigator = Navigator::new(&repo, crate::grammars(), &shared, &diff, side)
                .map(Arc::new)
                .map_err(|e| (FAILED, e.to_string()))?;
            self.state().hold(snapshots, side, Arc::clone(&navigator));
            navigator
        };
        let fit = match fit {
            Fit::Same => "same",
            Fit::Changes => "changes",
            Fit::New => "new",
        };
        let mut s = format!(
            r#"{{"navigator":{},"fit":"{fit}","fingerprint":"{:016x}","languages":["#,
            navigator.id(),
            navigator.fingerprint()
        );
        for (i, language) in navigator.languages().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            string(&mut s, language.name());
        }
        s.push_str(r#"],"progress":"#);
        wire::progress(navigator.progress(), &mut s);
        s.push('}');
        Ok(s)
    }

    /// The navigator a request names, and the asking file it names.
    fn navigator_and_origin(&self, params: &Value) -> Result<(Arc<Navigator>, Origin), Failure> {
        let id = number(params, "navigator").map_err(invalid)? as u64;
        let (path, language) =
            wire::read_origin(field(params, "origin").map_err(invalid)?).map_err(invalid)?;
        let language = (crate::grammars().named(&language))
            .filter(|l| l.has_tags())
            .ok_or_else(|| invalid(format!("no language {language} with definitions")))?
            .clone();
        let navigator = self.state().touch(id)?;
        Ok((navigator, Origin { path, language }))
    }

    /// `{"navigator","name","origin"}`: where the name is defined.
    fn lookup(&self, params: &Value, cancel: &AtomicBool) -> Result<String, Failure> {
        let name = text(params, "name").map_err(invalid)?.to_owned();
        let (navigator, origin) = self.navigator_and_origin(params)?;
        let (repo, cache) = {
            let state = self.state();
            let setup = state.setup()?;
            (setup.repo.clone(), setup.cache.symbols())
        };
        let found = navigator
            .lookup(&repo, &cache, &name, &origin, cancel)
            .map_err(|e| failed(&e))?;
        let mut s = String::from(r#"{"candidates":["#);
        for (i, c) in found.candidates.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            wire::candidate(c, &mut s);
        }
        let _ = write!(s, r#"],"total":{},"progress":"#, found.total);
        wire::progress(found.progress, &mut s);
        s.push('}');
        Ok(s)
    }

    /// `{"navigator"}`: how far its index is built, whether its build runs,
    /// a tags query that doesn't compile, and the files left without
    /// definitions.
    fn progress(&self, params: &Value) -> Result<String, Failure> {
        let id = number(params, "navigator").map_err(invalid)? as u64;
        let state = self.state();
        state.setup()?;
        let held = (state.navigators.iter())
            .find(|h| h.navigator.id() == id)
            .ok_or_else(|| (GONE, format!("no navigator {id}; open it again")))?;
        let building = (held.build.as_ref()).is_some_and(|(_, thread)| !thread.is_finished());
        let error = held.error.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(format!(
            "{{{}}}",
            built(&held.navigator, building, error.as_deref())
        ))
    }

    /// `{"navigator","kind":"references"|"text","name","origin","cap"?}`: a
    /// search, stepped by `search/step`.
    fn start(&self, params: &Value, cancel: &AtomicBool) -> Result<String, Failure> {
        let name = text(params, "name").map_err(invalid)?.to_owned();
        let kind = text(params, "kind").map_err(invalid)?.to_owned();
        let (navigator, origin) = self.navigator_and_origin(params)?;
        let repo = self.state().setup()?.repo.clone();
        let search = match kind.as_str() {
            "references" => Search::References {
                search: navigator
                    .references(&repo, &name, &origin, cancel)
                    .map_err(|e| failed(&e))?,
                navigator,
                found: 0,
            },
            "text" => {
                let cap = params.get("cap").and_then(Value::as_usize).unwrap_or(50);
                Search::Text {
                    search: navigator
                        .text_matches(&repo, &name, &origin, cap, cancel)
                        .map_err(|e| failed(&e))?,
                    navigator,
                    found: 0,
                }
            }
            other => return Err(invalid(format!("no search kind {other}"))),
        };
        let mut state = self.state();
        state.next_search += 1;
        let id = state.next_search;
        let of = search.navigator().id();
        state.searches.push((id, of, Arc::new(Mutex::new(search))));
        if state.searches.len() > SEARCHES {
            state.searches.remove(0);
        }
        Ok(format!(r#"{{"search":{id}}}"#))
    }

    /// `{"search","budget_ms","until"?,"past_cap"?}`: the next files of a
    /// search, for at most `budget_ms`, at least one, until it has `until`
    /// lines in all; a search for uses stops at its listed files' cap
    /// unless `past_cap`.
    fn step(&self, params: &Value, cancel: &AtomicBool) -> Result<String, Failure> {
        let id = number(params, "search").map_err(invalid)? as u64;
        let budget = Duration::from_millis(number(params, "budget_ms").map_err(invalid)? as u64);
        let until = params.get("until").and_then(Value::as_usize);
        let past_cap = !matches!(params.get("past_cap"), Some(Value::Bool(false)));
        let (search, repo) = {
            let state = self.state();
            let search = (state.searches.iter())
                .find(|(s, ..)| *s == id)
                .map(|(.., s)| Arc::clone(s))
                .ok_or_else(|| (GONE, format!("no search {id}; start it again")))?;
            (search, state.setup()?.repo.without_object_cache())
        };
        let mut search = lock(&search);
        let navigator = Arc::clone(search.navigator());
        self.state().touch(navigator.id())?;
        let indexed = navigator.progress().complete();
        let started = Instant::now();
        let mut first = true;
        let mut s = String::from(r#"{"hits":["#);
        let (files, searched, done) = match &mut *search {
            Search::References { search, found, .. } => {
                let mut hits = Vec::new();
                // A file a step at least, so a zero budget still goes on.
                loop {
                    let full = until.is_some_and(|u| *found >= u)
                        || (!past_cap && search.files() >= MAX_REFERENCE_FILES);
                    if search.done() || full {
                        break;
                    }
                    let mut stepped = false;
                    let got =
                        search.step(&navigator, &repo, || std::mem::replace(&mut stepped, true));
                    *found += got.len();
                    hits.extend(got);
                    if started.elapsed() >= budget || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                }
                for (i, hit) in hits.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    wire::reference(hit, &mut s);
                }
                (search.files(), search.searched(), search.done())
            }
            Search::Text { search, found, .. } => {
                let got = search.step(&navigator, &repo, || {
                    !std::mem::take(&mut first)
                        && (started.elapsed() >= budget || cancel.load(Ordering::Relaxed))
                });
                *found += got.len();
                for (i, hit) in got.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    wire::candidate(hit, &mut s);
                }
                (0, search.searched(), search.done())
            }
        };
        let _ = write!(s, r#"],"files":{files},"searched":"#);
        wire::progress(searched, &mut s);
        let _ = write!(s, r#","done":{done},"indexed":{indexed}}}"#);
        Ok(s)
    }
}

impl Search {
    fn navigator(&self) -> &Arc<Navigator> {
        match self {
            Self::References { navigator, .. } | Self::Text { navigator, .. } => navigator,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failed(e: &navigate::Error) -> Failure {
    match e {
        navigate::Error::Cancelled => (CANCELLED, "cancelled".to_owned()),
        e => (FAILED, e.to_string()),
    }
}
