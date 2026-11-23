//! A host's handle on its navigator helper: starts `dffy __navigate` in the
//! repository, lowered in priority, and makes its requests, each blocking
//! the calling thread until it's answered, as a worker job or a tool call
//! waits on it. How far an index is built comes as the helper pushes it.

use std::fmt::Write as _;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::Duration;

use dffy_core::cache::Cache;
use dffy_core::diff::Whitespace;
use dffy_core::git::Snapshot;
use dffy_core::navigate::{Candidate, Filter, Lookup, Progress, Reference, Use};
use dffy_core::view::Side;

use super::wire::{self, field, flag, number};
use super::{COMMAND, PROTOCOL};
use crate::plugin::json::{Value, string};
use crate::plugin::rpc::{self, Pending};

/// JSON-RPC codes a host acts on: a navigator or search the helper no
/// longer holds, and a request it stopped.
pub(crate) const GONE: i32 = -32001;
pub(crate) const CANCELLED: i32 = -32800;

/// How long `close` waits for the helper to exit before killing it: not
/// at all, as it keeps nothing the disk cache doesn't.
const GRACE: Duration = Duration::ZERO;

/// How long a cancelled request waits for the helper to say it stopped
/// before it ends as cancelled all the same.
const CANCEL_WAIT: Duration = Duration::from_secs(2);

/// A request the helper didn't answer with a result: its error's code, or
/// 0 when it never answered, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub code: i32,
    pub message: String,
}

impl Refused {
    /// The helper never answered: it failed to start, or exited.
    fn other(message: String) -> Self {
        Self { code: 0, message }
    }

    fn cancelled() -> Self {
        Self {
            code: CANCELLED,
            message: "cancelled".to_owned(),
        }
    }

    /// An answer the host can't read.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taken as `map_err` hands it, from the readers' `String` errors"
    )]
    fn unreadable(message: String) -> Self {
        Self {
            code: -1,
            message: format!("the navigator helper's answer: {message}"),
        }
    }

    /// The helper is gone: a host starts another for its next request.
    pub(crate) fn helper_gone(&self) -> bool {
        self.code == 0
    }
}

/// A flag that stops what waits on the helper: setting it runs what was
/// hung on it, as a request's own cancel, at once.
#[derive(Default)]
pub(crate) struct Cancel {
    set: AtomicBool,
    hooks: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cancel")
            .field("set", &self.is_set())
            .finish_non_exhaustive()
    }
}

impl Cancel {
    pub(crate) fn cancel(&self) {
        let hooks = {
            let mut hooks = self.hooks.lock().unwrap_or_else(PoisonError::into_inner);
            self.set.store(true, Ordering::Relaxed);
            std::mem::take(&mut *hooks)
        };
        for hook in hooks {
            hook();
        }
    }

    pub(crate) fn is_set(&self) -> bool {
        self.set.load(Ordering::Relaxed)
    }

    /// Runs `hook` once it's set: now, if it is.
    fn on_cancel(&self, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = self.hooks.lock().unwrap_or_else(PoisonError::into_inner);
        if self.set.load(Ordering::Relaxed) {
            drop(hooks);
            hook();
        } else {
            hooks.push(Box::new(hook));
        }
    }
}

/// The program the helper runs as: this one, unless a test or benchmark
/// set the dffy binary it built ([`use_program`]).
static PROGRAM: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Runs helpers as `program` from now on, for tests and benchmarks, whose
/// own executable isn't dffy.
pub(crate) fn use_program(program: PathBuf) {
    PROGRAM.get_or_init(|| program);
}

fn program() -> io::Result<PathBuf> {
    match PROGRAM.get() {
        Some(program) => Ok(program.clone()),
        None => std::env::current_exe(),
    }
}

/// How far a navigator's index is built, as the helper pushes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pushed {
    pub navigator: u64,
    pub built: Built,
}

/// What the client keeps under its lock: `initialize`'s params, and where
/// pushed progress goes.
pub(crate) struct Setup {
    threads: usize,
    cache: (PathBuf, u64),
    pushed: Option<Box<dyn Fn(Pushed) + Send>>,
}

/// What a request's answer, or its cancel, wakes its waiter with.
enum Woken {
    Answer(Result<Value, Refused>),
    Cancelled,
}

type Reply = mpsc::Sender<Woken>;

const KIND: rpc::Kind<(), Setup, Reply> = rpc::Kind {
    thread: "dffy-navigate",
    protocol: PROTOCOL,
    init_params,
    grace: GRACE,
    // One that exited, idle or not, is started again on the host's next ask.
    restarts: false,
    initialized: |_, result| match result.get("protocol").and_then(Value::as_usize) {
        Some(PROTOCOL) => Ok(()),
        other => Err(format!(
            "the navigator helper speaks protocol {other:?}, not {PROTOCOL}"
        )),
    },
    answered,
    notified,
    failed: |pending, why| {
        pending
            .request
            .send(Woken::Answer(Err(Refused::other(why))))
            .ok();
    },
};

fn init_params(setup: &Setup) -> String {
    initialize_members(setup.threads, &setup.cache.0, setup.cache.1)
}

/// The members `initialize` takes past `protocol` and `client`, each after
/// a comma.
pub(crate) fn initialize_members(threads: usize, dir: &Path, bytes: u64) -> String {
    let mut s = format!(r#","threads":{threads},"cache":{{"dir":"#);
    string(&mut s, &dir.to_string_lossy());
    let _ = write!(s, r#","bytes":{bytes}}}"#);
    s
}

fn answered(
    _: &rpc::Shared<(), Setup, Reply>,
    inner: rpc::Guard<'_, (), Setup, Reply>,
    Pending { request, .. }: Pending<Reply>,
    value: &Value,
) {
    drop(inner);
    let outcome = match (value.get("result"), value.get("error")) {
        (_, Some(error)) if *error != Value::Null => Err(Refused {
            code: code(error.get("code")),
            message: (error.get("message").and_then(Value::as_str))
                .unwrap_or("an error without a message")
                .to_owned(),
        }),
        (Some(result), _) => Ok(result.clone()),
        (None, _) => Err(Refused::other("an answer with nothing in it".to_owned())),
    };
    request.send(Woken::Answer(outcome)).ok();
}

/// A `progress` notification: handed to the host's callback, if it gave
/// one; anything else is dropped.
fn notified(
    _: &rpc::Shared<(), Setup, Reply>,
    inner: rpc::Guard<'_, (), Setup, Reply>,
    method: &str,
    params: &Value,
) {
    if method != "progress" {
        return;
    }
    let (Some(pushed), Ok(navigator), Ok(built)) = (
        &inner.kind.pushed,
        number(params, "navigator"),
        read_progress(params),
    ) else {
        return;
    };
    pushed(Pushed {
        navigator: navigator as u64,
        built,
    });
    drop(inner);
}

/// An error object's code; 0 when it has no whole one.
fn code(value: Option<&Value>) -> i32 {
    match value {
        Some(Value::Number(n)) if n.fract() == 0.0 && n.abs() < 1e9 => {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "checked: whole and well inside i32"
            )]
            let code = *n as i32;
            code
        }
        _ => 0,
    }
}

/// A navigator the helper holds, as `open` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Opened {
    pub navigator: u64,
    /// `same`, `changes` or `new`, as the core's refit said.
    pub fit: String,
    /// What it answers from, as the core's fingerprint has it.
    pub fingerprint: String,
    pub languages: Vec<String>,
    pub progress: Progress,
}

/// How far a navigator's index is built, as `progress` answers and the
/// helper pushes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Built {
    pub progress: Progress,
    /// Its build runs: an incomplete index without one isn't being built.
    pub building: bool,
    /// A tags query that doesn't compile.
    pub error: Option<String>,
    /// Files with no definitions: over 512KB, too slow to parse, and
    /// unreadable.
    pub skipped: [usize; 3],
    /// Why the helper keeps the index in memory rather than in its store
    /// file, if it does.
    pub note: Option<String>,
}

/// A step of a search: what it found, and how far it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step<T> {
    pub hits: Vec<T>,
    /// Files with a use so far; 0 for a text search.
    pub files: usize,
    pub searched: Progress,
    pub done: bool,
    /// The navigator's index was whole when the step started.
    pub indexed: bool,
}

/// A running helper. Cloning shares the process.
#[derive(Clone)]
pub(crate) struct Helper(rpc::Client<(), Setup, Reply>);

impl Helper {
    /// Starts the helper in the repository at `root`, its index in `cache`,
    /// built on `threads` threads, behind the user's own work through
    /// `nice` where that's installed. What it pushes of how far an index is
    /// built goes to `pushed`, on the client's reading thread.
    pub(crate) fn start(
        root: &Path,
        cache: &Cache,
        threads: usize,
        pushed: Option<Box<dyn Fn(Pushed) + Send>>,
    ) -> io::Result<Self> {
        let root = root.to_owned();
        let command: rpc::MakeCommand = Box::new(move || {
            let exe = program()?;
            let mut command = match on_path("nice") {
                Some(nice) => {
                    let mut c = Command::new(nice);
                    c.args(["-n", "10"]).arg(&exe);
                    c
                }
                None => Command::new(&exe),
            };
            command.arg(COMMAND).current_dir(&root);
            Ok(command)
        });
        let setup = Setup {
            threads,
            cache: (cache.dir().to_owned(), cache.budget()),
            pushed,
        };
        rpc::Client::spawn(
            "the navigator helper".to_owned(),
            command,
            None,
            KIND,
            setup,
        )
        .map(Self)
    }

    /// Asks the helper to exit and kills it at once, with what it started:
    /// nothing it holds is lost that the disk cache doesn't keep.
    pub(crate) fn close(&self) {
        self.0.close();
    }

    /// The helper's process, for tests that stop it.
    #[cfg(test)]
    pub(crate) fn pid(&self) -> Option<rustix::process::Pid> {
        self.0.leader()
    }

    /// Sends `method` with `params`, a JSON object, and waits for its
    /// answer. `cancel`, once set, asks the helper to stop it, and the
    /// request ends cancelled when the helper doesn't answer within
    /// [`CANCEL_WAIT`].
    pub(crate) fn request(
        &self,
        method: &'static str,
        params: String,
        cancel: Option<&Cancel>,
    ) -> Result<Value, Refused> {
        let (tx, rx) = mpsc::channel();
        let waker = tx.clone();
        let run = self.0.request(method, tx, move |_, _| Some(params));
        let gone = || Refused::other("the navigator helper went away".to_owned());
        if let Some(cancel) = cancel {
            cancel.on_cancel(move || {
                waker.send(Woken::Cancelled).ok();
            });
        } else {
            drop(waker);
        }
        match rx.recv() {
            Ok(Woken::Answer(outcome)) => outcome,
            Ok(Woken::Cancelled) => {
                run.cancel();
                match rx.recv_timeout(CANCEL_WAIT) {
                    Ok(Woken::Answer(outcome)) => outcome,
                    Ok(Woken::Cancelled) | Err(_) => Err(Refused::cancelled()),
                }
            }
            Err(_) => Err(gone()),
        }
    }

    pub(crate) fn open(
        &self,
        snapshots: [Snapshot; 2],
        side: Side,
        whitespace: Whitespace,
    ) -> Result<Opened, Refused> {
        let result = self.request("open", open_params(snapshots, side, whitespace), None)?;
        read_opened(&result).map_err(Refused::unreadable)
    }

    /// Where `name` is defined, used at `at` in the asking file when that's
    /// known.
    pub(crate) fn lookup(
        &self,
        navigator: u64,
        name: &str,
        (path, language): (&[u8], &str),
        at: Option<Use>,
        cancel: Option<&Cancel>,
    ) -> Result<Lookup, Refused> {
        let mut extra = String::new();
        wire::at(at, &mut extra);
        let params = asking_params(navigator, name, (path, language), &extra);
        let result = self.request("lookup", params, cancel)?;
        read_lookup(&result).map_err(Refused::unreadable)
    }

    /// Starts a search for `name`'s uses (`references`, the uses `filter`
    /// keeps) or its text matches (`text`, at most `cap` lines); its id.
    pub(crate) fn start_search(
        &self,
        navigator: u64,
        kind: &str,
        name: &str,
        origin: (&[u8], &str),
        (cap, filter): (usize, Filter),
    ) -> Result<u64, Refused> {
        let mut extra = search_extra(kind, cap);
        wire::filter(filter, &mut extra);
        let params = asking_params(navigator, name, origin, &extra);
        let result = self.request("search/start", params, None)?;
        number(&result, "search")
            .map(|s| s as u64)
            .map_err(Refused::unreadable)
    }

    pub(crate) fn step_references(
        &self,
        search: u64,
        (budget, until, past_cap): (Duration, Option<usize>, bool),
        cancel: Option<&Cancel>,
    ) -> Result<Step<Reference>, Refused> {
        let params = step_params(search, budget, until, past_cap);
        let result = self.request("search/step", params, cancel)?;
        read_step(&result, wire::read_reference).map_err(Refused::unreadable)
    }

    pub(crate) fn step_text(
        &self,
        search: u64,
        budget: Duration,
        cancel: Option<&Cancel>,
    ) -> Result<Step<Candidate>, Refused> {
        let params = step_params(search, budget, None, true);
        let result = self.request("search/step", params, cancel)?;
        read_step(&result, wire::read_candidate).map_err(Refused::unreadable)
    }

    /// Drops a search the host no longer steps: a notification, so it
    /// waits on nothing.
    pub(crate) fn close_search(&self, search: u64) {
        self.0
            .lock()
            .notify("search/close", &format!(r#"{{"search":{search}}}"#));
    }

    /// How far a navigator's index is built, whether its build runs, a tags
    /// query that doesn't compile, and the files left without definitions.
    pub(crate) fn progress(&self, navigator: u64) -> Result<Built, Refused> {
        let result = self.request("progress", format!(r#"{{"navigator":{navigator}}}"#), None)?;
        read_progress(&result).map_err(Refused::unreadable)
    }
}

/// Starts in a row past which a host stops starting helpers.
const MAX_STARTS: usize = 3;

/// The window those starts are counted in.
const STARTS_WINDOW: Duration = Duration::from_secs(60);

/// A host's helper across its restarts: the one running and its
/// generation, which tags every navigator and search id it hands out, so
/// an id from a helper gone never reaches another.
#[derive(Default)]
pub(crate) struct Helpers {
    running: Option<(u64, Helper)>,
    generation: u64,
    starts: Vec<std::time::Instant>,
}

impl Helpers {
    /// The helper running, with its generation.
    pub(crate) fn current(&self) -> Option<(u64, Helper)> {
        self.running.clone()
    }

    /// The helper to ask: the one running, unless it's of generation
    /// `gone`, which failed; then another, made by `start` for its
    /// generation, unless 3 started in the last minute.
    ///
    /// # Errors
    ///
    /// Why there's no helper, as a host says it: `definitions unavailable:
    /// …`.
    pub(crate) fn get(
        &mut self,
        gone: Option<(u64, &str)>,
        start: impl FnOnce(u64) -> io::Result<Helper>,
    ) -> Result<(u64, Helper), String> {
        match (&self.running, gone) {
            // Another took the place of the one that failed: it's asked.
            (Some((generation, helper)), Some((failed, _))) if *generation != failed => {
                return Ok((*generation, helper.clone()));
            }
            (Some((generation, helper)), None) => return Ok((*generation, helper.clone())),
            _ => {}
        }
        if let Some((_, old)) = self.running.take() {
            old.close();
        }
        let now = std::time::Instant::now();
        self.starts
            .retain(|at| now.saturating_duration_since(*at) < STARTS_WINDOW);
        if self.starts.len() >= MAX_STARTS {
            let why = gone.map_or("", |(_, why)| why);
            return Err(format!(
                "definitions unavailable: the helper keeps exiting: {why}"
            ));
        }
        self.starts.push(now);
        self.generation += 1;
        let helper = start(self.generation).map_err(|e| format!("definitions unavailable: {e}"))?;
        self.running = Some((self.generation, helper.clone()));
        Ok((self.generation, helper))
    }

    /// Closes the helper running, which gives its memory back.
    pub(crate) fn close(&mut self) {
        if let Some((_, helper)) = self.running.take() {
            helper.close();
        }
    }
}

/// The first `program` among `PATH`'s absolute folders that can be run.
fn on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|path| {
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// What `search/start` takes beside the asking: the kind and the cap.
pub(crate) fn search_extra(kind: &str, cap: usize) -> String {
    let mut extra = String::from(r#","kind":"#);
    string(&mut extra, kind);
    let _ = write!(extra, r#","cap":{cap}"#);
    extra
}

pub(crate) fn open_params(snapshots: [Snapshot; 2], side: Side, whitespace: Whitespace) -> String {
    let mut s = String::from(r#"{"snapshots":["#);
    wire::snapshot(snapshots[0], &mut s);
    s.push(',');
    wire::snapshot(snapshots[1], &mut s);
    let _ = write!(
        s,
        r#"],"side":"{}","whitespace":"{}"}}"#,
        wire::side(side),
        wire::whitespace(whitespace)
    );
    s
}

pub(crate) fn asking_params(
    navigator: u64,
    name: &str,
    (path, language): (&[u8], &str),
    extra: &str,
) -> String {
    let mut s = format!(r#"{{"navigator":{navigator},"name":"#);
    string(&mut s, name);
    s.push_str(r#","origin":"#);
    wire::origin(path, language, &mut s);
    s.push_str(extra);
    s.push('}');
    s
}

pub(crate) fn step_params(
    search: u64,
    budget: Duration,
    until: Option<usize>,
    past_cap: bool,
) -> String {
    let mut s = format!(
        r#"{{"search":{search},"budget_ms":{},"past_cap":{past_cap}"#,
        budget.as_millis()
    );
    if let Some(until) = until {
        let _ = write!(s, r#","until":{until}"#);
    }
    s.push('}');
    s
}

pub(crate) fn read_opened(result: &Value) -> Result<Opened, String> {
    Ok(Opened {
        navigator: number(result, "navigator")? as u64,
        fit: wire::text(result, "fit")?.to_owned(),
        fingerprint: wire::text(result, "fingerprint")?.to_owned(),
        languages: (field(result, "languages")?.as_array())
            .ok_or("languages isn't a list")?
            .iter()
            .map(|l| {
                l.as_str()
                    .map(str::to_owned)
                    .ok_or("a language isn't a string")
            })
            .collect::<Result<_, _>>()?,
        progress: wire::read_progress(field(result, "progress")?)?,
    })
}

pub(crate) fn read_lookup(result: &Value) -> Result<Lookup, String> {
    Ok(Lookup {
        candidates: (field(result, "candidates")?.as_array())
            .ok_or("candidates isn't a list")?
            .iter()
            .map(wire::read_candidate)
            .collect::<Result<_, _>>()?,
        total: number(result, "total")?,
        progress: wire::read_progress(field(result, "progress")?)?,
    })
}

pub(crate) fn read_step<T>(
    result: &Value,
    hit: fn(&Value) -> Result<T, String>,
) -> Result<Step<T>, String> {
    Ok(Step {
        hits: (field(result, "hits")?.as_array())
            .ok_or("hits isn't a list")?
            .iter()
            .map(hit)
            .collect::<Result<_, _>>()?,
        files: number(result, "files")?,
        searched: wire::read_progress(field(result, "searched")?)?,
        done: flag(result, "done")?,
        indexed: flag(result, "indexed")?,
    })
}

pub(crate) fn read_progress(result: &Value) -> Result<Built, String> {
    let error = match field(result, "error")? {
        Value::Null => None,
        other => Some(other.as_str().ok_or("error isn't a string")?.to_owned()),
    };
    let skipped = field(result, "skipped")?;
    Ok(Built {
        progress: wire::read_progress(field(result, "progress")?)?,
        building: flag(result, "building")?,
        error,
        skipped: [
            number(skipped, "too_large")?,
            number(skipped, "timed_out")?,
            number(skipped, "unreadable")?,
        ],
        note: match field(result, "note")? {
            Value::Null => None,
            other => Some(other.as_str().ok_or("note isn't a string")?.to_owned()),
        },
    })
}
