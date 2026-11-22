//! The agent plugin (experimental): `dffy-agent` on PATH, which asks a
//! coding agent the user already runs about a review thread and hands back
//! its answer, for dffy to write as the agent's reply, and says how to
//! start each agent on a thread for Fix with agent. One long-lived process
//! per viewer, a [`Plugin`], started on the first ask or handoff, or by
//! `dffy doctor`, and never as dffy starts, speaking JSON-RPC 2.0 over its
//! stdin and stdout through an [`rpc::Client`]; docs/plugins.md has the
//! messages. dffy names no agent: what it runs is what the plugin says.
//!
//! Its `initialize` answer isn't kept in the disk cache: the agents it
//! lists are the ones installed now, which a cached answer would hide.

use std::ffi::OsString;
use std::io;
#[cfg(test)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::Program;
use super::json::{self, Value};
use super::review::{self, Check};
use super::rpc::{self, Pending};

/// The program, found on PATH as review plugins are.
pub(crate) const PROGRAM: &str = "dffy-agent";

/// The argument a handoff's prompt takes the place of.
const PROMPT: &str = "{prompt}";

/// The protocol version, sent in `initialize` and named in its answer; a
/// plugin of another version is refused.
pub(crate) const PROTOCOL: usize = 1;

/// The plugin, in an absolute directory of PATH.
pub(crate) fn find() -> Option<Program> {
    super::find("dffy-", "agent")
}

/// How long `close` waits for the plugin to exit: it kills the asks still
/// running first, each in a process group of its own.
const GRACE: Duration = Duration::from_millis(500);

/// How long the plugin's `doctor` may take: it probes the agents' versions
/// and logins side by side, each probe for 2 seconds at most, so one that
/// doesn't answer is said in its own row before this runs out.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(3);

/// One of the agents `initialize` lists: those that can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Agent {
    /// `claude`, `codex` or `custom`: lowercase letters, digits, `-` and
    /// `_`, as a mention names it.
    pub name: String,
    pub default: bool,
    /// How its replies are labeled, as `read-only sandbox`.
    pub mark: Option<String>,
}

/// How a thread is handed to an agent `initialize` lists: run
/// interactively, as the user runs it, on the thread's prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handoff {
    pub name: String,
    /// Absolute.
    pub program: PathBuf,
    /// Its arguments: one that's [`PROMPT`] is where the prompt goes.
    pub args: Vec<String>,
    /// The prompt goes on the clipboard for the user to paste, as the
    /// agent takes its first message from them.
    pub clipboard: bool,
}

impl Handoff {
    fn decode(value: &Value) -> Result<Self, String> {
        let name = text(value, "name").unwrap_or_default();
        super::check_name(&name).map_err(|e| format!("a handoff's name: {e}"))?;
        let program = PathBuf::from(text(value, "program").unwrap_or_default());
        if !program.is_absolute() {
            return Err(format!("{name}'s program isn't an absolute path"));
        }
        let args = match value.get("args") {
            None => Vec::new(),
            Some(args) => (args.as_array())
                .and_then(|args| args.iter().map(|a| a.as_str().map(str::to_owned)).collect())
                .ok_or_else(|| format!("{name}'s args aren't text"))?,
        };
        let clipboard = match value.get("prompt") {
            None | Some(Value::Null) => false,
            Some(Value::String(at)) if at == "clipboard" => true,
            Some(at) => {
                let at = at.as_str().unwrap_or("not text");
                return Err(format!(
                    "{name}'s prompt is {at}, and dffy knows only clipboard"
                ));
            }
        };
        if !clipboard && !args.iter().any(|a| a == PROMPT) {
            return Err(format!(
                "{name}'s handoff takes no prompt: no {PROMPT} argument, and not on the clipboard"
            ));
        }
        Ok(Self {
            name,
            program,
            args,
            clipboard,
        })
    }

    /// Its program and arguments, `prompt` in place of [`PROMPT`].
    pub(crate) fn command(&self, prompt: &str) -> Vec<OsString> {
        std::iter::once(self.program.clone().into_os_string())
            .chain(
                self.args
                    .iter()
                    .map(|arg| OsString::from(if arg == PROMPT { prompt } else { arg.as_str() })),
            )
            .collect()
    }
}

/// What `initialize` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Init {
    pub agents: Vec<Agent>,
    /// The agents a thread can be handed to, in the order the first is the
    /// default; some answer no ask.
    pub handoffs: Vec<Handoff>,
}

impl Init {
    fn decode(result: &Value) -> Result<Self, String> {
        match result.get("protocol").map(Value::as_usize) {
            Some(Some(PROTOCOL)) => {}
            Some(Some(other)) => {
                return Err(format!(
                    "it speaks agent protocol {other}, and dffy speaks {PROTOCOL}"
                ));
            }
            _ => return Err("its initialize names no protocol".to_owned()),
        }
        let listed = (result.get("agents").and_then(Value::as_array))
            .ok_or("its initialize lists no agents")?;
        let agents = listed
            .iter()
            .map(|agent| {
                let name = text(agent, "name").unwrap_or_default();
                super::check_name(&name).map_err(|e| format!("an agent's name: {e}"))?;
                let mark = match agent.get("mark") {
                    None | Some(Value::Null) => None,
                    Some(mark) => Some(
                        (mark.as_str())
                            .filter(|m| printable(m, 60))
                            .ok_or_else(|| {
                                format!("{name}'s mark isn't 1 to 60 printable characters")
                            })?
                            .to_owned(),
                    ),
                };
                Ok(Agent {
                    default: agent.get("default") == Some(&Value::Bool(true)),
                    name,
                    mark,
                })
            })
            .collect::<Result<_, String>>()?;
        let handoffs = match result.get("handoffs") {
            None | Some(Value::Null) => Vec::new(),
            Some(listed) => (listed.as_array())
                .ok_or("its handoffs aren't a list")?
                .iter()
                .map(Handoff::decode)
                .collect::<Result<_, String>>()?,
        };
        Ok(Self { agents, handoffs })
    }

    /// The agent a thread is handed to: the one `config`, the config's
    /// `agent`, names, else the first listed. A command of the user's in
    /// `config`, a value with a space, takes no thread, so the first does.
    pub(crate) fn handoff(&self, config: Option<&str>) -> Result<&Handoff, String> {
        let named = config
            .map(str::trim)
            .filter(|c| !c.contains(char::is_whitespace));
        let Some(name) = named else {
            return self.handoffs.first().ok_or_else(|| {
                format!("{PROGRAM} lists no agent to hand the thread to; dffy doctor says why")
            });
        };
        self.handoffs.iter().find(|h| h.name == name).ok_or_else(|| {
            let listed: Vec<&str> = self.handoffs.iter().map(|h| h.name.as_str()).collect();
            if listed.is_empty() {
                format!(
                    "no agent named {name} to hand the thread to; {PROGRAM} lists none, dffy doctor says why"
                )
            } else {
                format!(
                    "no agent named {name} to hand the thread to; {}",
                    listed.join(", ")
                )
            }
        })
    }

    /// The agent `name` names, or the default one without a name.
    pub(crate) fn pick(&self, name: Option<&str>) -> Result<&Agent, String> {
        let Some(name) = name else {
            return (self.agents.iter().find(|a| a.default))
                .or_else(|| self.agents.first())
                .ok_or_else(|| "no agent can answer; dffy doctor says why".to_owned());
        };
        self.agents.iter().find(|a| a.name == name).ok_or_else(|| {
            let names: Vec<&str> = self.agents.iter().map(|a| a.name.as_str()).collect();
            if names.is_empty() {
                format!("no agent named {name}; none can answer, dffy doctor says why")
            } else {
                format!("no agent named {name}; {}", names.join(", "))
            }
        })
    }
}

/// `text` is 1 to `most` characters, none of them a control character.
fn printable(text: &str, most: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= most && !text.contains(char::is_control)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// What dffy asks the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// `prompt` to the agent `agent` names, or without one to the default
    /// agent; it reads the working tree the plugin was started in.
    Ask {
        agent: Option<String>,
        prompt: String,
    },
    Doctor,
}

impl Request {
    fn method(&self) -> &'static str {
        match self {
            Self::Ask { .. } => "ask",
            Self::Doctor => "doctor",
        }
    }
}

/// An agent's answer: its text, and the lines it proposes in place of the
/// commented ones, when it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub text: String,
    pub suggestion: Option<String>,
}

/// What a request says, in order; [`Said::Answered`] ends an ask and
/// [`Said::Checks`] a doctor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Said {
    /// The agent an ask went to, before anything else of it.
    Asking(Agent),
    /// What the agent is doing: `thinking…`.
    Progress(String),
    Answered(Result<Answer, String>),
    Checks(Result<Vec<Check>, String>),
    /// The plugin isn't on PATH: nothing was asked.
    NotInstalled,
}

/// A running agent plugin: one process, started again when it crashes,
/// that answers requests. Requests made before it has initialized wait for
/// it. Cloning shares the process.
#[derive(Clone)]
pub(crate) struct Plugin(rpc::Client<Init, State, Ask>);

/// A request made, to cancel: it still ends, saying it was cancelled.
pub(crate) type Run = rpc::Run<Init, State, Ask>;

type Shared = rpc::Shared<Init, State, Ask>;
type Guard<'a> = rpc::Guard<'a, Init, State, Ask>;

const KIND: rpc::Kind<Init, State, Ask> = rpc::Kind {
    thread: "dffy-agent",
    protocol: PROTOCOL,
    init_params,
    grace: GRACE,
    initialized: |_, result| Init::decode(result),
    answered,
    notified,
    failed: |mut pending, why| pending.request.end(Err(why)),
};

/// What dffy keeps of the plugin, under its client's lock.
pub(crate) struct State {
    /// The config's `agent`, passed in `initialize` as it is.
    config: Option<String>,
    /// The working tree an ask's agent reads, absolute.
    cwd: String,
}

/// A request, waiting for its answer.
pub(crate) struct Ask {
    method: &'static str,
    deliver: Box<dyn FnMut(Said) + Send>,
}

impl Ask {
    /// Ends the request with what `outcome` says: an ask's answer, or a
    /// doctor's checks, read from `outcome`'s result.
    fn end(&mut self, outcome: Result<&Value, String>) {
        let said = match self.method {
            "doctor" => Said::Checks(outcome.and_then(review::checks)),
            _ => Said::Answered(outcome.and_then(answer)),
        };
        (self.deliver)(said);
    }
}

fn init_params(state: &State) -> String {
    let Some(agent) = &state.config else {
        return String::new();
    };
    let mut out = String::from(",\"agent\":");
    json::string(&mut out, agent);
    out
}

/// An `ask` result: its text, and its suggestion when it has one.
fn answer(result: &Value) -> Result<Answer, String> {
    let text = text(result, "text").ok_or("its answer has no text")?;
    let suggestion = match result.get("suggestion") {
        None | Some(Value::Null) => None,
        Some(suggestion) => Some(
            suggestion
                .as_str()
                .ok_or("its suggestion isn't text")?
                .to_owned(),
        ),
    };
    Ok(Answer { text, suggestion })
}

impl Plugin {
    /// Starts `program` detached from the terminal, in `root`, the working
    /// tree its agents read, or where dffy runs. `config` is the config's
    /// `agent`, which it's given unread.
    pub(crate) fn start(
        program: &Program,
        root: Option<&Path>,
        config: Option<String>,
    ) -> io::Result<Self> {
        let cwd = root.map(|r| r.to_string_lossy().into_owned());
        Self::spawn(
            rpc::detached(program, root),
            State {
                config,
                cwd: cwd.unwrap_or_default(),
            },
        )
    }

    fn spawn(command: rpc::MakeCommand, state: State) -> io::Result<Self> {
        rpc::Client::spawn(PROGRAM.to_owned(), command, None, KIND, state).map(Self)
    }

    /// Hands the agents to `ready` once the plugin has initialized, or why
    /// it can't: now, if it has.
    pub(crate) fn on_ready(&self, ready: impl FnOnce(Result<Vec<Agent>, String>) + Send + 'static) {
        (self.0).on_ready(move |init| ready(init.map(|i| i.agents.clone())));
    }

    /// Initialized, and not since exited.
    pub(crate) fn ready(&self) -> bool {
        self.0.lock().ready()
    }

    /// Hands `ready` the agent a thread goes to, picked with `config`, the
    /// config's `agent`, as [`Init::handoff`] picks, once the plugin has
    /// initialized, or why there's none: now, if it has, else from a thread
    /// of its own, which says so when `initialize` doesn't answer in
    /// [`rpc::DESCRIBE_TIMEOUT`] and drops an answer after that.
    pub(crate) fn on_handoff(
        &self,
        config: Option<String>,
        ready: impl FnOnce(Result<Handoff, String>) + Send + 'static,
    ) {
        self.when_ready(move |i| i.handoff(config.as_deref()).cloned(), ready);
    }

    /// Hands `ready` what `read` makes of `initialize`'s answer once the
    /// plugin has initialized, or why there's none, as
    /// [`Plugin::on_handoff`] does.
    fn when_ready<T: Send + 'static>(
        &self,
        read: impl FnOnce(&Init) -> Result<T, String> + Send + 'static,
        ready: impl FnOnce(Result<T, String>) + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        (self.0).on_ready(move |init| {
            tx.send(init.and_then(read)).ok();
        });
        if let Ok(to) = rx.try_recv() {
            return ready(to);
        }
        std::thread::spawn(move || {
            let to = match rx.recv_timeout(rpc::DESCRIBE_TIMEOUT) {
                Ok(to) => to,
                Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                    "{PROGRAM}'s initialize didn't answer in {}s",
                    rpc::DESCRIBE_TIMEOUT.as_secs()
                )),
                // Closed, as dffy quits.
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            ready(to);
        });
    }

    /// Sends `request`, and each thing it says to `deliver` from a thread of
    /// the plugin's; `deliver` mustn't call back into the plugin. An ask
    /// names its agent once the plugin has initialized, which decides the
    /// default, and ends at once when no listed agent has that name; it
    /// waits for that as a handoff does ([`Plugin::on_handoff`]), and ends
    /// saying so when `initialize` doesn't answer in time.
    pub(crate) fn request(
        &self,
        request: &Request,
        deliver: impl FnMut(Said) + Send + 'static,
    ) -> Run {
        let asked = request.clone();
        let params = move |shared: &Shared, id: u64| {
            let mut inner = shared.lock();
            let Request::Ask { agent, prompt } = &asked else {
                return Some("{}".to_owned());
            };
            let picked = (inner.init.as_ref())
                .ok_or_else(|| "it hasn't initialized".to_owned())
                .and_then(|init| init.pick(agent.as_deref()).cloned());
            match picked {
                Ok(agent) => {
                    let mut params = String::from("{\"agent\":");
                    json::string(&mut params, &agent.name);
                    params.push_str(",\"prompt\":");
                    json::string(&mut params, prompt);
                    params.push_str(",\"cwd\":");
                    json::string(&mut params, &inner.kind.cwd);
                    params.push('}');
                    if let Some(pending) = inner.pending.get_mut(&id) {
                        (pending.request.deliver)(Said::Asking(agent));
                    }
                    Some(params)
                }
                Err(why) => {
                    if let Some(mut pending) = inner.pending.remove(&id) {
                        pending.request.end(Err(why));
                    }
                    None
                }
            }
        };
        let ask = Ask {
            method: request.method(),
            deliver: Box::new(deliver),
        };
        let run = self.0.request(request.method(), ask, params);
        if matches!(request, Request::Ask { .. }) && !self.ready() {
            let waiting = run.clone();
            self.when_ready(
                |_| Ok(()),
                move |ready| {
                    if let Err(why) = ready {
                        waiting.give_up(why);
                    }
                },
            );
        }
        run
    }

    /// Asks the plugin to shut down, which stops every ask still running,
    /// waits a moment for it to exit, then kills it with whatever it
    /// started.
    pub(crate) fn close(&self) {
        self.0.close();
    }

    /// Has a panic close the plugin as [`Plugin::close`] does, before the
    /// panic hook's kill, which misses the agents it runs: each is in a
    /// process group of its own.
    pub(crate) fn close_on_panic(&self) {
        self.0.shut_down_on_panic();
    }
}

/// A response to `pending`'s request, which it ends.
fn answered(shared: &Shared, inner: Guard<'_>, mut pending: Pending<Ask>, value: &Value) {
    drop(inner);
    let method = pending.request.method;
    let outcome = match (value.get("result"), value.get("error")) {
        (_, Some(error)) if *error != Value::Null => {
            Err(rpc::error_message(shared.name(), method, error))
        }
        (Some(result), _) => Ok(result),
        (None, _) => Err(format!(
            "{} {method}: an answer without a result",
            shared.name()
        )),
    };
    pending.request.end(outcome);
}

/// A notification from the plugin: an ask's progress, a few printable
/// words, which dffy shows as they are.
fn notified(_: &Shared, mut inner: Guard<'_>, method: &str, params: &Value) {
    let request = params.get("request").and_then(rpc::request_id);
    let progress = text(params, "progress").filter(|p| printable(p, 40));
    if let (Some(id), "progress", Some(progress)) = (request, method, progress)
        && let Some(pending) = inner.pending.get_mut(&id).filter(|p| !p.ended)
    {
        (pending.request.deliver)(Said::Progress(progress));
    }
}

/// The plugin's checks of the agents, for `dffy doctor`: a process of its
/// own, which probes them again. `config` is the config's `agent`.
pub(crate) fn doctor(program: &Program, config: Option<String>) -> Result<Vec<Check>, String> {
    let plugin =
        Plugin::start(program, None, config).map_err(|e| format!("couldn't start it: {e}"))?;
    let (tx, rx) = mpsc::channel();
    plugin.request(&Request::Doctor, move |said| {
        tx.send(said).ok();
    });
    let until = Instant::now() + DOCTOR_TIMEOUT;
    let checks = loop {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Said::Checks(checks)) => break checks,
            Ok(_) => {}
            Err(_) => {
                break Err(format!(
                    "its doctor didn't answer in {}s",
                    DOCTOR_TIMEOUT.as_secs()
                ));
            }
        }
    };
    plugin.close();
    checks
}

#[cfg(test)]
mod tests;
