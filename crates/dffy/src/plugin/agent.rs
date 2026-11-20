//! The agent plugin (experimental): `dffy-agent` on PATH, which asks a
//! coding agent the user already runs about a review thread and hands back
//! its answer, for dffy to write as the agent's reply. One long-lived
//! process per viewer, a [`Plugin`], started on the first ask or by `dffy
//! doctor` and never as dffy starts, speaking JSON-RPC 2.0 over its stdin
//! and stdout through an [`rpc::Client`]; docs/plugins.md has the messages.
//!
//! Its `initialize` answer isn't kept in the disk cache: the agents it
//! lists are the ones installed now, which a cached answer would hide.

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

/// The agent CLIs a thread can be handed to, run as the user runs them,
/// the first found the default. Amp answers no ask, and takes its first
/// message only on stdin, which an interactive session has for the user.
const INTERACTIVE: [&str; 4] = ["claude", "codex", "pi", "amp"];

/// Whether interactive agent `name` starts on a prompt given as its last
/// argument; the others start bare.
pub(crate) fn takes_prompt(name: &str) -> bool {
    name != "amp"
}

/// The coding agent a thread is handed to, to run interactively with no
/// flags of dffy's, and its program, as `find` finds it on PATH: the one
/// the config's `agent` names when it's one of claude, codex, pi and amp,
/// else the first of those found. The plugin isn't involved, and a command
/// of the user's in `agent`, which takes a question on stdin rather than a
/// session to work in, isn't one.
pub(crate) fn interactive(
    config: Option<&str>,
    find: impl Fn(&str) -> Option<PathBuf>,
) -> Result<(&'static str, PathBuf), String> {
    let named = |name: &str| INTERACTIVE.into_iter().find(|n| *n == name);
    if let Some(name) = config.and_then(named) {
        return find(name)
            .map(|path| (name, path))
            .ok_or_else(|| format!("{name} isn't on PATH"));
    }
    INTERACTIVE
        .into_iter()
        .find_map(|name| Some((name, find(name)?)))
        .ok_or_else(|| "none of claude, codex, pi and amp is on PATH".to_owned())
}

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

/// How long the plugin's `doctor` may take: it probes each agent's
/// version and login.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(10);

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

/// What `initialize` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Init {
    pub agents: Vec<Agent>,
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
        Ok(Self { agents })
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

    /// Sends `request`, and each thing it says to `deliver` from a thread of
    /// the plugin's; `deliver` mustn't call back into the plugin. An ask
    /// names its agent once the plugin has initialized, which decides the
    /// default, and ends at once when no listed agent has that name.
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
        self.0.request(request.method(), ask, params)
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
                    "its doctor took over {}s",
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
