//! The plugin's side of dffy's review plugin protocol: JSON-RPC 2.0 over
//! stdin and stdout, one message a line, for as long as dffy keeps the
//! process. Each request is handled on a thread of its own, so a dry run
//! answers while a `prepare` imports; a `post` waits for the imports still
//! running and holds the next ones back. While dffy shows a review and you
//! aren't idle, a watcher asks GitHub about it on its own cadence and
//! pushes what changed.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The protocol this speaks, which `initialize` must name.
pub(crate) const PROTOCOL: u64 = 1;

/// JSON-RPC's codes for what the request itself got wrong.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// The protocol's code for a request dffy cancelled.
const CANCELLED: i64 = -32800;

/// How often the review dffy shows is asked about while you're there.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// Why a request failed: what to say, with what was done before it, and
/// the JSON-RPC code.
pub(crate) struct Failure {
    pub message: String,
    pub data: Option<Value>,
    pub code: i64,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            data: None,
            code: 1,
        }
    }
}

impl Failure {
    pub(crate) fn params(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            data: None,
            code: INVALID_PARAMS,
        }
    }
}

/// A request being handled: where its notifications go, whether it was
/// cancelled, and whether it was answered early.
pub(crate) struct Out {
    id: Value,
    cancelled: Arc<AtomicBool>,
    answered: AtomicBool,
}

impl Out {
    /// The review's session, as soon as its commits are in the clone.
    pub(crate) fn session(&self, session: &Value) {
        notify("session", &json!({"request": self.id, "session": session}));
    }

    /// How far it got: `100/312`.
    pub(crate) fn progress(&self, progress: &str) {
        notify(
            "progress",
            &json!({"request": self.id, "progress": progress}),
        );
    }

    /// Has dffy write `ops` to the review's comments file, in the
    /// vocabulary of `dffy comments apply`, all or none, before it reads
    /// anything this request says after.
    pub(crate) fn apply(&self, ops: &[Value]) {
        if !ops.is_empty() {
            notify("comments/apply", &json!({"request": self.id, "ops": ops}));
        }
    }

    /// `Err` once dffy cancelled the request; work stops at the next one.
    pub(crate) fn go_on(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".to_owned());
        }
        Ok(())
    }

    /// Answers now, for work that goes on after it without dffy waiting.
    pub(crate) fn answer(&self, result: &Value) {
        if !self.answered.swap(true, Ordering::Relaxed) {
            write(&json!({"jsonrpc": "2.0", "id": self.id, "result": result}));
        }
    }
}

static STDOUT: Mutex<()> = Mutex::new(());

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One message, a line, whole: handlers write from threads of their own.
fn write(message: &Value) {
    let _guard = lock(&STDOUT);
    let mut stdout = std::io::stdout().lock();
    // dffy may be gone, and the work goes on.
    writeln!(stdout, "{message}").ok();
    stdout.flush().ok();
}

fn notify(method: &str, params: &Value) {
    write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
}

/// `post` waits for this alone; imports share it.
static POSTING: RwLock<()> = RwLock::new(());

/// Holds the lane a request runs in: a `post` alone, imports side by side.
pub(crate) fn exclusive() -> std::sync::RwLockWriteGuard<'static, ()> {
    POSTING.write().unwrap_or_else(PoisonError::into_inner)
}

/// An import's share of the lane, which also holds the watcher back.
pub(crate) struct Importing(
    #[expect(dead_code, reason = "held")] std::sync::RwLockReadGuard<'static, ()>,
);

pub(crate) fn importing() -> Importing {
    IMPORTS.fetch_add(1, Ordering::Relaxed);
    Importing(POSTING.read().unwrap_or_else(PoisonError::into_inner))
}

impl Drop for Importing {
    fn drop(&mut self) {
        IMPORTS.fetch_sub(1, Ordering::Relaxed);
    }
}

static IMPORTS: AtomicUsize = AtomicUsize::new(0);

/// What the watcher knows of the review dffy shows.
#[derive(Default)]
struct Watch {
    review: Option<u64>,
    idle: bool,
    /// When it next asks GitHub.
    due: Option<Instant>,
    /// The head it last said the review is on, when not the session's.
    moved: Option<String>,
    /// It said the review changed since its comments were imported.
    changed: bool,
    /// Why it last failed, said once until that changes.
    failed: Option<String>,
}

static WATCH: LazyLock<(Mutex<Watch>, Condvar)> = LazyLock::new(Default::default);

/// Says the review's head on GitHub isn't the session's, as a warm
/// `prepare` finds it, unless that was said already.
pub(crate) fn moved(pr: u64, head: &str) {
    let mut watch = lock(&WATCH.0);
    if watch.review == Some(pr) && watch.moved.as_deref() == Some(head) {
        return;
    }
    if watch.review == Some(pr) {
        watch.moved = Some(head.to_owned());
    }
    notify("head/moved", &json!({"id": pr.to_string(), "head": head}));
}

/// What the watcher finds when it asks about `pr`: its head on GitHub,
/// whether that isn't the session's, and whether the review changed since
/// its comments were imported.
pub(crate) struct Looked {
    pub head: String,
    pub moved: bool,
    pub changed: bool,
}

/// Asks about the review dffy shows every [`CHECK_EVERY`], and as soon as
/// you're back from idle, pushing what changed since it last said; it
/// waits while you're idle, while nothing is shown, and while an import
/// runs.
fn watcher(look: fn(u64) -> Result<Looked, String>) {
    let (state, wake) = &*WATCH;
    loop {
        let mut watch = lock(state);
        let pr = loop {
            match (watch.review, watch.due) {
                (Some(pr), Some(due)) if !watch.idle => {
                    let now = Instant::now();
                    if now >= due {
                        break pr;
                    }
                    watch = wake
                        .wait_timeout(watch, due - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                _ => watch = wake.wait(watch).unwrap_or_else(PoisonError::into_inner),
            }
        };
        watch.due = Some(Instant::now() + CHECK_EVERY);
        if IMPORTS.load(Ordering::Relaxed) > 0 {
            continue;
        }
        drop(watch);
        let looked = look(pr);
        let mut watch = lock(state);
        if watch.review != Some(pr) {
            continue;
        }
        match looked {
            Ok(looked) => {
                watch.failed = None;
                let moved = looked.moved.then(|| looked.head.clone());
                if moved != watch.moved {
                    watch.moved = moved;
                    notify(
                        "head/moved",
                        &json!({"id": pr.to_string(), "head": looked.head}),
                    );
                }
                if looked.changed && !watch.changed {
                    notify("comments/changed", &json!({"id": pr.to_string()}));
                }
                watch.changed = looked.changed;
            }
            Err(e) if watch.failed.as_ref() != Some(&e) => {
                notify(
                    "log",
                    &json!({"message": format!("checking #{pr} failed: {e}")}),
                );
                watch.failed = Some(e);
            }
            Err(_) => {}
        }
    }
}

/// A notification from dffy; `false` for `exit`.
fn heard(method: &str, params: &Value, cancels: &Mutex<HashMap<String, Arc<AtomicBool>>>) -> bool {
    let (state, wake) = &*WATCH;
    match method {
        "$/cancelRequest" => {
            if let Some(flag) = lock(cancels).get(&params["id"].to_string()) {
                flag.store(true, Ordering::Relaxed);
            }
        }
        "watch" => {
            let review = params["id"].as_str().and_then(|id| id.parse().ok());
            let mut watch = lock(state);
            if watch.review != review {
                *watch = Watch {
                    review,
                    idle: watch.idle,
                    due: Some(Instant::now() + CHECK_EVERY),
                    ..Watch::default()
                };
                wake.notify_all();
            }
        }
        "idle/changed" => {
            let idle = params["idle"] == true;
            let mut watch = lock(state);
            if watch.idle && !idle {
                watch.due = Some(Instant::now());
            }
            watch.idle = idle;
            wake.notify_all();
        }
        "exit" => return false,
        _ => {}
    }
    true
}

/// What a handler is given: the method and its params, with the request's
/// [`Out`].
pub(crate) type Handler = fn(&str, &Value, &Out) -> Option<Result<Value, Failure>>;

/// Serves dffy on stdin and stdout until it says `exit` or closes stdin.
/// `initialize` answers with `describe`'s fields; `handle` answers every
/// other request, `None` for a method it doesn't have. `look` is the
/// watcher's question.
pub(crate) fn serve(
    describe: fn() -> Value,
    handle: Handler,
    look: fn(u64) -> Result<Looked, String>,
) {
    std::thread::Builder::new()
        .name("watch".into())
        .spawn(move || watcher(look))
        .ok();
    let cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
    let running = Arc::new((Mutex::new(0_usize), Condvar::new()));
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            write(&json!({"jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": "not JSON"}}));
            continue;
        };
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = message.get("id").cloned() else {
            if heard(&method, &params, &cancels) {
                continue;
            }
            return;
        };
        match method.as_str() {
            "initialize" => {
                let answer = initialize(&params, describe);
                respond(&id, answer);
                continue;
            }
            // Answered once every request still running is.
            "shutdown" => {
                let (count, done) = &*running;
                let mut count = lock(count);
                while *count > 0 {
                    count = done.wait(count).unwrap_or_else(PoisonError::into_inner);
                }
                respond(&id, Ok(Value::Null));
                continue;
            }
            _ => {}
        }
        let flag = Arc::new(AtomicBool::new(false));
        lock(&cancels).insert(id.to_string(), Arc::clone(&flag));
        *lock(&running.0) += 1;
        let (cancels, running) = (Arc::clone(&cancels), Arc::clone(&running));
        std::thread::spawn(move || {
            let out = Out {
                id: id.clone(),
                cancelled: flag,
                answered: AtomicBool::new(false),
            };
            let answer = handle(&method, &params, &out).unwrap_or_else(|| {
                Err(Failure {
                    message: format!("dffy-review-github has no {method}"),
                    data: None,
                    code: METHOD_NOT_FOUND,
                })
            });
            let answer = answer.map_err(|failure| {
                if out.cancelled.load(Ordering::Relaxed) {
                    Failure {
                        message: "cancelled".to_owned(),
                        data: failure.data,
                        code: CANCELLED,
                    }
                } else {
                    failure
                }
            });
            match answer {
                Ok(_) if out.answered.load(Ordering::Relaxed) => {}
                answer => respond(&id, answer),
            }
            lock(&cancels).remove(&id.to_string());
            let (count, done) = &*running;
            *lock(count) -= 1;
            done.notify_all();
        });
    }
}

fn initialize(params: &Value, describe: fn() -> Value) -> Result<Value, Failure> {
    match params["protocol"].as_u64() {
        Some(PROTOCOL) => {}
        asked => {
            let asked = asked.map_or_else(|| "none".to_owned(), |p| p.to_string());
            return Err(Failure::params(format!(
                "dffy-review-github speaks review protocol {PROTOCOL}, and dffy asked for \
                 {asked}; install the dffy-review-github that matches your dffy"
            )));
        }
    }
    let mut answer = describe();
    answer["protocol"] = json!(PROTOCOL);
    answer["capabilities"] = json!({"watch": true});
    Ok(answer)
}

fn respond(id: &Value, answer: Result<Value, Failure>) {
    write(&match answer {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(Failure {
            message,
            data,
            code,
        }) => {
            let mut error = json!({"code": code, "message": message});
            if let Some(data) = data {
                error["data"] = data;
            }
            json!({"jsonrpc": "2.0", "id": id, "error": error})
        }
    });
}
