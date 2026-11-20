//! `dffy-agent`, an experimental agent plugin for dffy: it asks a coding
//! agent you already run (Claude Code, Codex, Pi, or a command of yours)
//! about a review thread, and hands its answer back for dffy to put in the
//! thread. dffy runs it in the background, speaking agent protocol 1 on its
//! stdin and stdout (see [`server`]); it never prompts or draws, and never
//! writes a file. The agent reads the worktree and nothing more because of
//! how it's run, not because the prompt asks (see [`agent`]).

mod agent;
mod server;

use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use crate::agent::Agents;
use crate::server::{Failure, Out};

const USAGE: &str = "usage: dffy-agent [--version]; \
                     without arguments it serves dffy on stdin and stdout";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => {
            server::serve(Agents::probe, handle);
            ExitCode::SUCCESS
        }
        ["--version"] => {
            println!("dffy-agent {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Answers a request of dffy's; `None` for a method it doesn't have.
fn handle(
    method: &str,
    params: &Value,
    agents: &Agents,
    out: &Out,
) -> Option<Result<Value, Failure>> {
    Some(match method {
        "ask" => ask(params, agents, out),
        "doctor" => Ok(agents.doctor()),
        _ => return None,
    })
}

/// `ask {agent, prompt, cwd}`: `cwd` is the worktree the agent reads.
fn ask(params: &Value, agents: &Agents, out: &Out) -> Result<Value, Failure> {
    let field = |key: &str| {
        params[key]
            .as_str()
            .ok_or_else(|| Failure::params(format!("ask needs {key}")))
    };
    let (name, prompt, cwd) = (field("agent")?, field("prompt")?, Path::new(field("cwd")?));
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(Failure::params(format!(
            "{} isn't a directory by its absolute path",
            cwd.display()
        )));
    }
    agents.get(name)?.ask(prompt, cwd, out)
}
