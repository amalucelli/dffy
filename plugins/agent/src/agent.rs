//! The agents an ask can go to and their recipes: the arguments that keep
//! each one read-only, which the recipe enforces rather than the prompt
//! asks for. The recipes target the current release of each CLI, with no
//! version checks; a release that breaks one gets the recipe fixed here. A
//! custom command is the user's, flags and all.

use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use serde_json::{Value, json};

use crate::server::{FAILED, Failure, LOGGED_OUT, Out, UNAVAILABLE};

/// How long each of `doctor`'s probes may take: `--version`, and asking
/// whether it's logged in. dffy waits longer for the whole `doctor`, so a
/// probe that doesn't answer is said in its own row.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long asking an agent's CLI whether it's logged in may take, after
/// an ask failed.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

/// A built-in agent: how it's run so it only reads, and how what it prints
/// is read. To add one, follow "Add an agent recipe" in docs/design.md.
struct Recipe {
    /// Its program on `PATH`, and its `name` in `initialize`.
    name: &'static str,
    args: &'static [&'static str],
    /// The flag that names the worktree after `args`, for a CLI that
    /// doesn't take it from its directory alone.
    worktree_flag: Option<&'static str>,
    /// How its replies are labeled; `None` for none.
    mark: Option<&'static str>,
    /// What the recipe allows, for `doctor`.
    said: &'static str,
    /// `said` in a few words, for `dffy doctor`'s row of the agent.
    brief: &'static str,
    login: Login,
    /// What to run to log in, as a failed ask says it.
    log_in: &'static str,
    /// Reads one event of its stdout into the stream, the answer or failure
    /// included, by whatever rule the CLI's output needs: the progress to
    /// show, if any.
    event: fn(&mut Stream, &Value) -> Option<&'static str>,
}

/// How a failed run tells it isn't logged in.
enum Login {
    /// Its CLI's status command, which exits 1 when it isn't.
    Status(&'static [&'static str]),
    /// stderr lines holding one of these, as its start prints them.
    Stderr(&'static [&'static str]),
}

/// The built-in agents, in the order `initialize` lists them.
static RECIPES: [Recipe; 3] = [
    Recipe {
        name: "claude",
        // Only the tools that read and search: `--tools` leaves MCP
        // servers' tools, so they're denied apart and only the flag's
        // servers load, none; user settings only, so the reviewed
        // repository's hooks don't run.
        args: &[
            "-p",
            "--verbose",
            "--output-format",
            "stream-json",
            "--tools",
            "Read,Grep,Glob",
            "--disallowedTools",
            "mcp__*",
            "--strict-mcp-config",
            "--setting-sources",
            "user",
            "--no-session-persistence",
            "--permission-mode",
            "dontAsk",
        ],
        worktree_flag: None,
        mark: None,
        said: "read-only: Read, Grep and Glob only, MCP tools denied, user settings only",
        brief: "read-only: Read, Grep, Glob",
        login: Login::Status(&["auth", "status"]),
        log_in: "claude auth login",
        event: claude_event,
    },
    Recipe {
        name: "codex",
        // Its shell in its read-only sandbox, which the OS holds to no
        // writes and no network, and web search off. The user's own setup
        // loads as in their normal use, MCP servers and hooks included,
        // which run outside the sandbox.
        args: &[
            "exec",
            "--json",
            "--sandbox",
            "read-only",
            "--ephemeral",
            "-c",
            "web_search=\"disabled\"",
        ],
        worktree_flag: Some("-C"),
        mark: Some("read-only sandbox"),
        said: "read-only sandbox: commands may run, no writes, no network, web search off",
        brief: "read-only sandbox",
        login: Login::Status(&["login", "status"]),
        log_in: "codex login",
        event: codex_event,
    },
    Recipe {
        name: "pi",
        // Only the tools that read and search, with no MCP servers, the
        // reviewed repository's trust-gated config ignored, and no network
        // but the model's. No OS sandbox: `read` reaches any path, and the
        // user's own extensions still load.
        args: &[
            "--mode",
            "json",
            "--no-session",
            "--tools",
            "read,grep,find,ls",
            "--no-mcp",
            "--no-approve",
            "--offline",
        ],
        worktree_flag: None,
        mark: Some("read-only tools"),
        said: "read-only tools: read, grep, find and ls only, MCP off, \
               project config ignored, offline; no sandbox",
        brief: "read-only tools · no sandbox",
        login: Login::Stderr(&["No API key found", "No models available"]),
        log_in: "pi auth login <provider>",
        event: pi_event,
    },
];

/// How a review thread is handed to an agent run interactively, in its
/// normal mode with none of a recipe's flags, for Fix with agent in dffy:
/// its arguments, an item `{prompt}` being where dffy puts the thread's
/// prompt, or with `clipboard` none, dffy putting the prompt on the
/// clipboard for the user to paste.
struct Handoff {
    /// Its program on `PATH`, and its `name` in `initialize`.
    name: &'static str,
    args: &'static [&'static str],
    clipboard: bool,
}

/// The agents a thread can be handed to, in the order dffy picks the first
/// found. Amp has no recipe, as it has no read-only mode an ask could rely
/// on, and takes its first message only on stdin, which an interactive
/// session keeps for the user.
static HANDOFFS: [Handoff; 4] = [
    Handoff {
        name: "claude",
        args: &["{prompt}"],
        clipboard: false,
    },
    Handoff {
        name: "codex",
        args: &["{prompt}"],
        clipboard: false,
    },
    Handoff {
        name: "pi",
        args: &["{prompt}"],
        clipboard: false,
    },
    Handoff {
        name: "amp",
        args: &[],
        clipboard: true,
    },
];

/// An agent an ask can name.
pub(crate) struct Agent {
    name: &'static str,
    /// `None` for a custom command.
    recipe: Option<&'static Recipe>,
    /// Its program by absolute path, or why there's none.
    program: Result<PathBuf, String>,
    /// A custom command's arguments.
    args: Vec<String>,
    mark: Option<&'static str>,
}

/// The agents found, as `initialize` looked for them.
pub(crate) struct Agents {
    list: Vec<Agent>,
    /// The agents a thread can be handed to that are on `PATH`, each with
    /// its program.
    handoffs: Vec<(&'static Handoff, PathBuf)>,
    /// The config's `agent` value, as dffy passed it.
    configured: Option<String>,
}

impl Agents {
    /// Looks for each recipe's and handoff's program on `PATH`, and a
    /// custom command when `configured` is one: a value with a space. Any
    /// other value names the default agent. Nothing is run.
    pub(crate) fn probe(configured: Option<&str>) -> Self {
        let configured = configured.map(str::trim).filter(|c| !c.is_empty());
        let custom = configured.filter(|c| c.contains(char::is_whitespace));
        let mut list: Vec<Agent> = custom.map(custom_agent).into_iter().collect();
        list.extend(RECIPES.iter().map(built_in));
        let handoffs = (HANDOFFS.iter())
            .filter_map(|handoff| Some((handoff, on_path(handoff.name)?)))
            .collect();
        Self {
            list,
            handoffs,
            configured: configured.map(str::to_owned),
        }
    }

    /// `initialize`'s `handoffs`: how a thread is handed to each agent
    /// found, by its program's absolute path.
    pub(crate) fn handoffs(&self) -> Value {
        (self.handoffs.iter())
            .map(|(handoff, program)| {
                let mut said =
                    json!({"name": handoff.name, "program": program, "args": handoff.args});
                if handoff.clipboard {
                    said["prompt"] = json!("clipboard");
                }
                said
            })
            .collect()
    }

    fn ready(&self) -> impl Iterator<Item = &Agent> {
        self.list.iter().filter(|a| a.ready())
    }

    /// The agent an ask without a name goes to: a custom command, then the
    /// one the config names, then the first that can run.
    fn default(&self) -> Option<&'static str> {
        let named = self.configured.as_deref();
        self.ready()
            .find(|a| a.recipe.is_none())
            .or_else(|| self.ready().find(|a| Some(a.name) == named))
            .or_else(|| self.ready().next())
            .map(|a| a.name)
    }

    /// `initialize`'s `agents`: those that can run, with the default.
    pub(crate) fn listed(&self) -> Value {
        let default = self.default();
        self.ready()
            .map(|a| json!({"name": a.name, "default": Some(a.name) == default, "mark": a.mark}))
            .collect()
    }

    pub(crate) fn get(&self, name: &str) -> Result<&Agent, Failure> {
        self.list.iter().find(|a| a.name == name).ok_or_else(|| {
            let ready: Vec<&str> = self.ready().map(|a| a.name).collect();
            let ready = if ready.is_empty() {
                "none is installed".to_owned()
            } else {
                ready.join(", ")
            };
            Failure::params(format!("no agent named {name}; {ready}"))
        })
    }

    /// For `dffy doctor`: each agent found, looked for again, what its
    /// `--version` says, how its recipe runs it, failed when its `--help`
    /// doesn't list one of the recipe's flags, and whether it's logged in. An agent not installed fails only when the config names it, or
    /// when no agent can answer. A name the config gives an agent without
    /// a recipe, which only Fix with agent starts, gets a row of its own.
    /// The agents are probed side by side, each probe for
    /// [`PROBE_TIMEOUT`] at most, and their rows kept in order.
    pub(crate) fn doctor(&self) -> Value {
        let fresh = Self::probe(self.configured.as_deref());
        let mut checks = Vec::new();
        if let Some(named) = fresh.configured.as_deref().filter(|c| {
            !c.contains(char::is_whitespace) && !fresh.list.iter().any(|a| a.name == *c)
        }) {
            let row = if !HANDOFFS.iter().any(|h| h.name == named) {
                let names: Vec<String> = (RECIPES.iter().map(|r| r.name.to_owned()))
                    .chain(
                        (HANDOFFS.iter())
                            .filter(|h| !RECIPES.iter().any(|r| r.name == h.name))
                            .map(|h| format!("{} (Fix with agent only)", h.name)),
                    )
                    .collect();
                check(
                    "agent",
                    false,
                    &format!(
                        "{named} names no agent: {}, or a command and its arguments",
                        names.join(", ")
                    ),
                )
            } else if fresh.handoffs.iter().any(|(h, _)| h.name == named) {
                let asks =
                    (fresh.default()).map_or_else(String::new, |d| format!("; asks go to {d}"));
                check(
                    "agent",
                    true,
                    &format!("{named} is used for Fix with agent only{asks}"),
                )
            } else {
                check(
                    "agent",
                    false,
                    &format!("{named} is used for Fix with agent only, and isn't installed"),
                )
            };
            checks.push(row);
        }
        let any_ready = fresh.ready().next().is_some();
        let configured = fresh.configured.as_deref();
        let rows: Vec<Vec<Value>> = std::thread::scope(|scope| {
            let probes: Vec<_> = (fresh.list.iter())
                .map(|agent| scope.spawn(move || agent.checks(configured, any_ready)))
                .collect();
            probes
                .into_iter()
                .map(|probe| probe.join().unwrap_or_default())
                .collect()
        });
        checks.extend(rows.into_iter().flatten());
        json!({ "checks": checks })
    }
}

fn check(name: &str, ok: bool, detail: &str) -> Value {
    json!({"name": name, "ok": ok, "detail": detail})
}

/// A check with the few words `dffy doctor` folds into the agent's row,
/// given only when they differ from `detail`.
fn check_brief(name: &str, ok: bool, detail: &str, brief: &str) -> Value {
    let mut check = check(name, ok, detail);
    if brief != detail {
        check["brief"] = json!(brief);
    }
    check
}

/// The version in what `--version` printed: its first word with a digit,
/// as `2.1.329` in `2.1.329 (Claude Code)` or `0.44.0` in
/// `codex-cli 0.44.0`.
fn version_number(said: &str) -> &str {
    said.split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(said)
}

/// The recipe's flags that `help`, its CLI's `--help`, doesn't list.
fn unknown_flags(recipe: &Recipe, help: &str) -> Vec<&'static str> {
    let listed = |flag: &str| {
        help.match_indices(flag).any(|(at, _)| {
            let before = help[..at].chars().next_back();
            let after = help[at + flag.len()..].chars().next();
            let word = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
            !before.is_some_and(word) && !after.is_some_and(word)
        })
    };
    (recipe.args.iter().copied())
        .chain(recipe.worktree_flag)
        .filter(|arg| arg.starts_with('-') && !listed(arg))
        .collect()
}

/// A probe that didn't answer in its time, and was killed.
#[derive(Debug, PartialEq, Eq)]
struct Late;

/// What a row says of `command`, a probe that didn't answer.
fn late(command: &str) -> String {
    format!("`{command}` didn't answer in {}s", PROBE_TIMEOUT.as_secs())
}

/// An absolute `PATH` entry's executable `name`: a relative entry would
/// let the reviewed repository supply the agent.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| executable(p))
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn built_in(recipe: &'static Recipe) -> Agent {
    Agent {
        name: recipe.name,
        recipe: Some(recipe),
        program: on_path(recipe.name).ok_or_else(|| "not installed".to_owned()),
        args: Vec::new(),
        mark: recipe.mark,
    }
}

/// The first line `program --version` prints, when it runs and succeeds.
fn version(program: &Path) -> Result<Option<String>, Late> {
    let ran = run_for(Command::new(program).arg("--version"), PROBE_TIMEOUT)?;
    Ok(ran
        .filter(|(status, _)| status.success())
        .and_then(|(_, out)| {
            out.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_owned)
        }))
}

/// What the CLI's `--help` prints for the recipe's command: after the
/// recipe's words before its first flag, as `codex exec`. `None` when it
/// doesn't run or prints nothing.
fn help(program: &Path, recipe: &Recipe) -> Result<Option<String>, Late> {
    let command = recipe.args.iter().take_while(|a| !a.starts_with('-'));
    let ran = run_for(
        Command::new(program).args(command).arg("--help"),
        PROBE_TIMEOUT,
    )?;
    Ok(ran.map(|(_, out)| out).filter(|out| !out.trim().is_empty()))
}

/// The configured command: its first word a name on `PATH` or an absolute
/// path, then its arguments, split at spaces, with no shell between.
fn custom_agent(command: &str) -> Agent {
    let mut words = command.split_whitespace().map(str::to_owned);
    let program = words.next().unwrap_or_default();
    let found = if program.contains('/') {
        let path = PathBuf::from(&program);
        if path.is_absolute() && executable(&path) {
            Ok(path)
        } else {
            Err(format!("{program} isn't an executable at an absolute path"))
        }
    } else {
        on_path(&program).ok_or_else(|| format!("{program} isn't on PATH"))
    };
    Agent {
        name: "custom",
        recipe: None,
        program: found,
        args: words.collect(),
        mark: Some("custom"),
    }
}

fn kill_group(group: Pid) {
    // Gone already when it exited.
    rustix::process::kill_process_group(group, Signal::KILL).ok();
}

/// Runs `command` outside the worktree to its end, or kills it with its
/// group after `limit`: its status and stdout, `None` when it can't run.
fn run_for(command: &mut Command, limit: Duration) -> Result<Option<(ExitStatus, String)>, Late> {
    let Ok(child) = command
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    else {
        return Ok(None);
    };
    let group = Pid::from_child(&child);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || tx.send(child.wait_with_output()).ok());
    if let Ok(output) = rx.recv_timeout(limit) {
        return Ok(output.ok().map(|output| {
            (
                output.status,
                String::from_utf8_lossy(&output.stdout).into_owned(),
            )
        }));
    }
    kill_group(group);
    Err(Late)
}

/// What a run's stderr said, read to its end.
#[derive(Default)]
struct Stderr {
    /// Why it failed: its first line that starts with `error:`, as a usage
    /// error's does with a hint and the usage after it, else its last line
    /// that isn't blank.
    reason: Option<String>,
    /// Whether a line holds one of the recipe's logged-out words.
    logged_out: bool,
}

fn read_stderr(stream: impl Read, logged_out: &[&str]) -> Stderr {
    let (mut error, mut last, mut out) = (None, None, false);
    for line in BufReader::new(stream).split(b'\n') {
        let Ok(line) = line else { break };
        let line = String::from_utf8_lossy(line.trim_ascii());
        if line.is_empty() {
            continue;
        }
        out = out || logged_out.iter().any(|words| line.contains(words));
        let line: String = line.chars().take(200).collect();
        if error.is_none() && line.to_ascii_lowercase().starts_with("error:") {
            error = Some(line.clone());
        }
        last = Some(line);
    }
    Stderr {
        reason: error.or(last),
        logged_out: out,
    }
}

impl Agent {
    fn ready(&self) -> bool {
        self.program.is_ok()
    }

    fn log_in(&self) -> String {
        let how = self.recipe.map_or("", |r| r.log_in);
        format!("{} isn't logged in; run `{how}`", self.name)
    }

    /// Whether its CLI's status command says it's logged in, asked for
    /// `limit` at most; `None` when it didn't say, and for an agent without
    /// one.
    fn logged_in(&self, limit: Duration) -> Result<Option<bool>, Late> {
        let (Ok(program), Some(Login::Status(args))) =
            (&self.program, self.recipe.map(|r| &r.login))
        else {
            return Ok(None);
        };
        let ran = run_for(Command::new(program).args(*args), limit)?;
        Ok(ran.and_then(|(status, _)| match status.code() {
            Some(0) => Some(true),
            Some(1) => Some(false),
            _ => None,
        }))
    }

    /// `doctor`'s rows for it: why it can't run, a custom command as it
    /// runs, or a recipe's version, recipe and login, the probes side by
    /// side.
    fn checks(&self, configured: Option<&str>, any_ready: bool) -> Vec<Value> {
        let program = match &self.program {
            Ok(program) => program,
            Err(why) => {
                // Only news while another agent can answer and the config
                // doesn't ask for this one.
                let wanted = self.recipe.is_none() || configured == Some(self.name) || !any_ready;
                if wanted {
                    return vec![check(self.name, false, why)];
                }
                let mut note = check(self.name, true, why);
                note["info"] = json!(true);
                return vec![note];
            }
        };
        let Some(recipe) = self.recipe else {
            let command = std::iter::once(program.display().to_string())
                .chain(self.args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            return vec![check(self.name, true, &command)];
        };
        std::thread::scope(|scope| {
            // A CLI that only says so as a run fails has no check here.
            let auth = match recipe.login {
                Login::Status(args) => Some((args, scope.spawn(|| self.logged_in(PROBE_TIMEOUT)))),
                Login::Stderr(_) => None,
            };
            let help = scope.spawn(|| help(program, recipe));
            let version = match version(program) {
                Ok(Some(said)) => check_brief(self.name, true, &said, version_number(&said)),
                Ok(None) => check(self.name, false, "--version failed"),
                Err(Late) => check(self.name, false, &late(&format!("{} --version", self.name))),
            };
            let name = format!("{} recipe", self.name);
            // Help that couldn't be read says nothing against the recipe.
            let unknown = match help.join() {
                Ok(Ok(Some(help))) => unknown_flags(recipe, &help),
                _ => Vec::new(),
            };
            let recipe = if unknown.is_empty() {
                check_brief(&name, true, recipe.said, recipe.brief)
            } else {
                let detail = format!("doesn't know {}: upgrade {}", unknown.join(", "), self.name);
                check(&name, false, &detail)
            };
            let mut rows = vec![version, recipe];
            if let Some((args, probe)) = auth {
                let name = format!("{} auth", self.name);
                rows.push(match probe.join() {
                    Ok(Ok(Some(true))) => check(&name, true, "logged in"),
                    Ok(Ok(Some(false))) => check(&name, false, &self.log_in()),
                    Ok(Err(Late)) => check(
                        &name,
                        false,
                        &late(&format!("{} {}", self.name, args.join(" "))),
                    ),
                    Ok(Ok(None)) | Err(_) => {
                        check(&name, false, "its CLI didn't say whether it's logged in")
                    }
                });
            }
            rows
        })
    }

    /// Asks the agent `prompt` in `cwd`, the prompt on its stdin, telling
    /// dffy what it's doing as it goes: `{text, suggestion?}`.
    pub(crate) fn ask(&self, prompt: &str, cwd: &Path, out: &Out) -> Result<Value, Failure> {
        let program = self
            .program
            .as_ref()
            .map_err(|why| Failure::new(UNAVAILABLE, format!("{}: {why}", self.name)))?;
        let mut command = Command::new(program);
        match self.recipe {
            Some(recipe) => {
                command.args(recipe.args);
                if let Some(flag) = recipe.worktree_flag {
                    command.arg(flag).arg(cwd);
                }
            }
            None => {
                command.args(&self.args);
            }
        }
        let (status, stream, stderr) =
            self.run(command.current_dir(cwd), prompt.to_owned(), out)?;
        // The server answers it as cancelled; asking whether it's logged in
        // would only delay that.
        if out.cancelled() {
            return Err(Failure::new(FAILED, "cancelled"));
        }
        let ended = || match status.code() {
            _ if status.success() => "it answered nothing".to_owned(),
            Some(code) => format!("exited with status {code}"),
            None => "was killed".to_owned(),
        };
        let answer = match self.recipe {
            None if status.success() && !stream.text.trim().is_empty() => Ok(stream.text),
            None if status.success() => Err(ended()),
            None => Err(stderr.reason.unwrap_or_else(ended)),
            Some(_) => match (stream.failure, stream.answer) {
                (Some(failure), _) => Err(failure),
                (None, Some(answer)) => Ok(answer),
                (None, None) => Err(stream.error.or(stderr.reason).unwrap_or_else(ended)),
            },
        };
        match answer {
            Ok(answer) => {
                let (text, suggestion) = split_suggestion(&answer);
                let mut said = json!({ "text": text });
                if let Some(suggestion) = suggestion {
                    said["suggestion"] = json!(suggestion);
                }
                Ok(said)
            }
            Err(_) if stderr.logged_out || self.logged_in(AUTH_TIMEOUT) == Ok(Some(false)) => {
                Err(Failure::new(LOGGED_OUT, self.log_in()))
            }
            Err(why) => Err(Failure::new(FAILED, format!("{} failed: {why}", self.name))),
        }
    }

    /// Runs `command` in a process group of its own, which a cancel kills,
    /// with `input` on its stdin: its status, what its stdout said, and what
    /// its stderr did.
    fn run(
        &self,
        command: &mut Command,
        input: String,
        out: &Out,
    ) -> Result<(ExitStatus, Stream, Stderr), Failure> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| Failure::new(FAILED, format!("can't run {}: {e}", self.name)))?;
        let group = Pid::from_child(&child);
        if !out.started(group) {
            kill_group(group);
        }
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            kill_group(group);
            child.wait().ok();
            return Err(Failure::new(FAILED, "no pipes"));
        };
        // Apart from the reading, so a prompt larger than the pipe can't
        // wait on an agent that waits to be read.
        let writer = std::thread::spawn(move || {
            // An agent that exits without reading it all says why itself.
            stdin.write_all(input.as_bytes()).ok();
        });
        let logged_out = match self.recipe.map(|r| &r.login) {
            Some(Login::Stderr(words)) => *words,
            _ => &[][..],
        };
        let stderr_said = std::thread::spawn(move || read_stderr(stderr, logged_out));
        let mut stream = Stream::new(self.recipe);
        if self.recipe.is_none() {
            stream.say("thinking…", out);
        }
        for line in BufReader::new(stdout).split(b'\n') {
            let Ok(line) = line else { break };
            stream.line(&line, out);
        }
        // Reaped only once nothing can signal its group: until then the
        // exited leader keeps the group's id from being reused.
        let exited = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        while let Err(rustix::io::Errno::INTR) = rustix::process::waitid(WaitId::Pid(group), exited)
        {
        }
        out.ended();
        kill_group(group);
        let status = child
            .wait()
            .map_err(|e| Failure::new(FAILED, format!("{} vanished: {e}", self.name)))?;
        writer.join().ok();
        Ok((status, stream, stderr_said.join().unwrap_or_default()))
    }
}

/// What an agent's stdout said, read as it comes.
struct Stream {
    /// `None` for a custom command.
    recipe: Option<&'static Recipe>,
    /// The progress said last.
    said: &'static str,
    /// A custom command's whole output.
    text: String,
    /// The answer, as the recipe's events give it.
    answer: Option<String>,
    /// A failure that ends the run, whatever the answer.
    failure: Option<String>,
    /// An error said on the way, the failure when no answer came.
    error: Option<String>,
}

impl Stream {
    fn new(recipe: Option<&'static Recipe>) -> Self {
        Self {
            recipe,
            said: "",
            text: String::new(),
            answer: None,
            failure: None,
            error: None,
        }
    }

    fn say(&mut self, progress: &'static str, out: &Out) {
        if progress != self.said {
            out.progress(progress);
            self.said = progress;
        }
    }

    /// Takes one line: an event of the recipe's JSON lines, or a line of a
    /// custom command's reply.
    fn line(&mut self, line: &[u8], out: &Out) {
        let Some(recipe) = self.recipe else {
            self.text.push_str(&String::from_utf8_lossy(line));
            self.text.push('\n');
            return;
        };
        // Not an event: a warning printed to stdout.
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        if let Some(progress) = (recipe.event)(self, &event) {
            self.say(progress, out);
        }
    }
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

/// Claude's stream-json: the answer is the final `result` event, a failure
/// when it's marked an error.
fn claude_event(stream: &mut Stream, event: &Value) -> Option<&'static str> {
    match event["type"].as_str().unwrap_or_default() {
        "system" => Some("starting…"),
        "assistant" => {
            let content = event["message"]["content"].as_array();
            let has = |kind: &str| content.into_iter().flatten().any(|c| c["type"] == kind);
            if has("text") {
                Some("writing…")
            } else if has("thinking") {
                Some("thinking…")
            } else {
                None
            }
        }
        "result" => {
            let result = text(&event["result"]);
            if event["is_error"] == true {
                stream.failure =
                    Some(result.unwrap_or_else(|| {
                        event["subtype"].as_str().unwrap_or("an error").to_owned()
                    }));
            } else {
                stream.answer = result;
            }
            None
        }
        _ => None,
    }
}

/// Codex's JSONL: the answer is the last completed agent message; a failed
/// turn fails the run whatever came before, and an error event does when
/// no answer came.
fn codex_event(stream: &mut Stream, event: &Value) -> Option<&'static str> {
    match event["type"].as_str().unwrap_or_default() {
        "thread.started" => Some("starting…"),
        "turn.started" => Some("thinking…"),
        kind @ ("item.started" | "item.updated" | "item.completed") => {
            let item = &event["item"];
            match item["type"].as_str().unwrap_or_default() {
                "reasoning" => Some("thinking…"),
                "command_execution" => Some("running a command…"),
                "agent_message" => {
                    if kind == "item.completed" {
                        stream.answer = text(&item["text"]);
                    }
                    Some("writing…")
                }
                _ => None,
            }
        }
        "turn.failed" => {
            stream.failure = Some(
                text(&event["error"]["message"]).unwrap_or_else(|| "its turn failed".to_owned()),
            );
            None
        }
        "error" => {
            stream.error = text(&event["message"]);
            None
        }
        _ => None,
    }
}

/// Pi's JSON mode, which exits 0 whatever happened: the last assistant
/// message decides, as Pi retries a failed one itself. One that stopped on
/// an error, an abort or the length limit is a failure; any other is the
/// answer, its text blocks joined.
fn pi_event(stream: &mut Stream, event: &Value) -> Option<&'static str> {
    match event["type"].as_str().unwrap_or_default() {
        "session" => Some("starting…"),
        "message_update" => match event["assistantMessageEvent"]["type"]
            .as_str()
            .unwrap_or_default()
        {
            "thinking_start" | "thinking_delta" => Some("thinking…"),
            "text_start" | "text_delta" => Some("writing…"),
            _ => None,
        },
        "tool_execution_start" => Some(match event["toolName"].as_str() {
            Some("read") => "reading…",
            _ => "searching…",
        }),
        "message_end" if event["message"]["role"] == "assistant" => {
            let message = &event["message"];
            let stop = message["stopReason"].as_str().unwrap_or_default();
            if let "error" | "aborted" | "length" = stop {
                stream.answer = None;
                stream.failure = Some(
                    text(&message["errorMessage"]).unwrap_or_else(|| format!("it stopped: {stop}")),
                );
            } else {
                let answer = message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                stream.failure = None;
                stream.answer = (!answer.trim().is_empty()).then_some(answer);
            }
            None
        }
        _ => None,
    }
}

/// A fence's backticks, at most three spaces in, and what follows them.
fn fence(line: &str) -> Option<(usize, &str)> {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return None;
    }
    let ticks = rest.len() - rest.trim_start_matches('`').len();
    (ticks >= 3).then(|| (ticks, &rest[ticks..]))
}

/// The answer's last ```` ```suggestion ```` block taken out of its text:
/// the text without it, trimmed, and the block's lines without a final
/// line break. The block is closed by a fence at least as long as the one
/// that opened it, and a fence inside another block opens nothing.
fn split_suggestion(answer: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = answer.lines().collect();
    let mut block = None;
    let mut i = 0;
    while i < lines.len() {
        let Some((ticks, info)) = fence(lines[i]).filter(|(_, info)| !info.contains('`')) else {
            i += 1;
            continue;
        };
        let close = (i + 1..lines.len()).find(|&j| {
            fence(lines[j]).is_some_and(|(n, rest)| n >= ticks && rest.trim().is_empty())
        });
        let Some(close) = close else { break };
        if info.trim() == "suggestion" {
            block = Some((i, close));
        }
        i = close + 1;
    }
    let Some((open, close)) = block else {
        return (answer.trim().to_owned(), None);
    };
    let text = lines[..open]
        .iter()
        .chain(&lines[close + 1..])
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    (
        text.trim().to_owned(),
        Some(lines[open + 1..close].join("\n")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_in_another_block_opens_no_suggestion() {
        let answer = "Like this:\n\n````md\n```suggestion\nnot this\n```\n````\n\nDone.";
        assert_eq!(split_suggestion(answer), (answer.to_owned(), None));
    }

    #[test]
    fn the_last_suggestion_is_the_one_taken() {
        let answer = "First:\n```suggestion\na\n```\nBetter:\n```suggestion\nb\n```";
        assert_eq!(
            split_suggestion(answer),
            (
                "First:\n```suggestion\na\n```\nBetter:".to_owned(),
                Some("b".to_owned())
            )
        );
    }

    #[test]
    fn an_empty_suggestion_removes_the_lines() {
        assert_eq!(
            split_suggestion("Drop it.\n```suggestion\n```"),
            ("Drop it.".to_owned(), Some(String::new()))
        );
    }
}
