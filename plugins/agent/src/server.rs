//! The plugin's side of dffy's agent protocol: JSON-RPC 2.0 over stdin and
//! stdout, one message a line, for as long as dffy keeps the process. Each
//! request runs on a thread of its own, so a `doctor` answers while an
//! `ask` waits on its agent. Each agent runs in a process group of its own,
//! which `$/cancelRequest` kills, and which `shutdown`, `exit` and the end
//! of stdin kill for every ask still running: dffy kills the plugin's
//! group when it quits, and that doesn't reach them.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use rustix::process::{Pid, Signal};
use serde_json::{Value, json};

use crate::agent::Agents;

/// The protocol this speaks, which `initialize` must name.
pub(crate) const PROTOCOL: u64 = 1;

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// The protocol's code for a request dffy cancelled.
const CANCELLED: i64 = -32800;
/// The agent ran and failed; the message says why.
pub(crate) const FAILED: i64 = 1;
/// The agent failed and its CLI says it isn't logged in.
pub(crate) const LOGGED_OUT: i64 = 2;
/// The agent isn't on PATH.
pub(crate) const UNAVAILABLE: i64 = 3;

/// Why a request failed: what to say, and the JSON-RPC code.
pub(crate) struct Failure {
    pub message: String,
    pub code: i64,
}

impl Failure {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }

    pub(crate) fn params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
}

/// An ask's agent: whether dffy cancelled it, and its process group while
/// it runs.
#[derive(Default)]
struct Run {
    cancelled: bool,
    group: Option<Pid>,
}

/// The requests running, by id as JSON.
static RUNS: LazyLock<Mutex<HashMap<String, Run>>> = LazyLock::new(Mutex::default);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn kill_group(group: Pid) {
    // Gone already when it exited.
    rustix::process::kill_process_group(group, Signal::KILL).ok();
}

/// Cancels every request running, killing their agents.
fn cancel_all() {
    for run in lock(&RUNS).values_mut() {
        run.cancelled = true;
        if let Some(group) = run.group {
            kill_group(group);
        }
    }
}

/// A request being handled: where its notifications go, and its agent.
pub(crate) struct Out {
    id: Value,
    key: String,
}

impl Out {
    /// What the agent is doing, in a word or two: `thinking…`.
    pub(crate) fn progress(&self, progress: &str) {
        notify(
            "progress",
            &json!({"request": self.id, "progress": progress}),
        );
    }

    /// Records the agent's process group, for a cancel to kill; `false`
    /// when the request was cancelled already, and the caller kills it.
    pub(crate) fn started(&self, group: Pid) -> bool {
        let mut runs = lock(&RUNS);
        let run = runs.entry(self.key.clone()).or_default();
        if !run.cancelled {
            run.group = Some(group);
        }
        !run.cancelled
    }

    /// The agent exited, and isn't reaped yet, so its group's id can't
    /// have been reused: nothing kills it from here on.
    pub(crate) fn ended(&self) {
        if let Some(run) = lock(&RUNS).get_mut(&self.key) {
            run.group = None;
        }
    }

    pub(crate) fn cancelled(&self) -> bool {
        lock(&RUNS).get(&self.key).is_some_and(|r| r.cancelled)
    }
}

static STDOUT: Mutex<()> = Mutex::new(());

/// One message, a line, whole: handlers write from threads of their own.
fn write(message: &Value) {
    let _guard = lock(&STDOUT);
    let mut stdout = std::io::stdout().lock();
    // dffy may be gone; the request ends anyway.
    writeln!(stdout, "{message}").ok();
    stdout.flush().ok();
}

fn notify(method: &str, params: &Value) {
    write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
}

fn respond(id: &Value, answer: Result<Value, Failure>) {
    write(&match answer {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(Failure { message, code }) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    });
}

/// What a handler is given: the method and its params, the agents
/// `initialize` found, and the request's [`Out`].
pub(crate) type Handler = fn(&str, &Value, &Agents, &Out) -> Option<Result<Value, Failure>>;

/// Serves dffy on stdin and stdout until it says `exit` or closes stdin.
/// `initialize` probes the agents with `probe`, given the config's `agent`
/// value; `handle` answers every other request, `None` for a method it
/// doesn't have.
pub(crate) fn serve(probe: fn(Option<&str>) -> Agents, handle: Handler) {
    let mut agents: Option<Arc<Agents>> = None;
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
            match method.as_str() {
                "$/cancelRequest" => cancel(&params["id"]),
                "exit" => break,
                _ => {}
            }
            continue;
        };
        match method.as_str() {
            "initialize" => {
                let answer = initialize(&params, probe).map(|(found, answer)| {
                    agents = Some(Arc::new(found));
                    answer
                });
                respond(&id, answer);
                continue;
            }
            "shutdown" => {
                cancel_all();
                respond(&id, Ok(Value::Null));
                continue;
            }
            _ => {}
        }
        let agents = Arc::clone(agents.get_or_insert_with(|| Arc::new(probe(None))));
        let key = id.to_string();
        lock(&RUNS).insert(key.clone(), Run::default());
        std::thread::spawn(move || {
            let out = Out { id, key };
            let answer = handle(&method, &params, &agents, &out).unwrap_or_else(|| {
                Err(Failure::new(
                    METHOD_NOT_FOUND,
                    format!("dffy-agent has no {method}"),
                ))
            });
            // However the agent ended once it was killed.
            let answer = if out.cancelled() {
                Err(Failure::new(CANCELLED, "cancelled"))
            } else {
                answer
            };
            respond(&out.id, answer);
            lock(&RUNS).remove(&out.key);
        });
    }
    cancel_all();
}

fn cancel(id: &Value) {
    if let Some(run) = lock(&RUNS).get_mut(&id.to_string()) {
        run.cancelled = true;
        if let Some(group) = run.group {
            kill_group(group);
        }
    }
}

fn initialize(
    params: &Value,
    probe: fn(Option<&str>) -> Agents,
) -> Result<(Agents, Value), Failure> {
    match params["protocol"].as_u64() {
        Some(PROTOCOL) => {}
        asked => {
            let asked = asked.map_or_else(|| "none".to_owned(), |p| p.to_string());
            return Err(Failure::params(format!(
                "dffy-agent speaks agent protocol {PROTOCOL}, and dffy asked for {asked}; \
                 install the dffy-agent that matches your dffy"
            )));
        }
    }
    let agents = probe(params["agent"].as_str());
    let answer = json!({
        "protocol": PROTOCOL,
        "agents": agents.listed(),
        "capabilities": {},
    });
    Ok((agents, answer))
}
