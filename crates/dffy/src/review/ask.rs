//! Asking a coding agent about a thread, through the agent plugin
//! ([`crate::plugin::agent`]): which threads wait on an agent, what each
//! ask said so far, and the answer written as the agent's reply in its
//! thread, through the comments' one write path. A frontend makes the
//! thread's prompt, hands the ask here, and carries out the [`Effect`]s
//! that come back, as for the review session.
//!
//! Only the user asks: with the ask key, or a comment of theirs, written in
//! the viewer, that starts with `@<name>`. A comment that comes any other
//! way (imported from a host, `dffy comments apply`, the control API, MCP)
//! never does, so whoever can comment on a review can't run an agent on
//! the reviewer's machine.

use super::model::Comments;
use super::session::{Effect, plural, read_back};
use super::{Change, Comment, Op, Target};
use crate::plugin::agent::{self, Agent, Answer, Request, Said};

/// The asks going on, and the agents the plugin listed.
#[derive(Default)]
pub(crate) struct Asking {
    /// The agents that can answer, as the plugin listed them when it
    /// initialized; `None` until then, or when it couldn't.
    agents: Option<Vec<Agent>>,
    /// In the order they were asked; one per thread.
    asks: Vec<Ask>,
    /// The last ask's id.
    run_id: u64,
}

struct Ask {
    id: u64,
    /// The thread's first comment.
    root: String,
    /// The agent asked, once the plugin picked it.
    agent: Option<Agent>,
    /// What it's doing, as the plugin last said.
    progress: Option<String>,
    /// The comment starting with `@<name>` it comes of, which may name a
    /// person rather than an agent. It's written kept local, as a mention
    /// on the host would reach whoever has that name there; when no agent
    /// takes the ask, it's a draft again. Without the plugin nothing is
    /// said; a plugin with no agent of that name says so.
    mention: Option<String>,
}

impl Ask {
    fn name(&self) -> &str {
        self.agent.as_ref().map_or("the agent", |a| a.name.as_str())
    }

    /// `asking claude… thinking…`.
    fn words(&self) -> String {
        let mut words = match &self.agent {
            Some(agent) => format!("asking {}…", agent.name),
            None => "asking…".to_owned(),
        };
        if let Some(progress) = &self.progress {
            words.push(' ');
            words.push_str(progress);
        }
        words
    }
}

impl Asking {
    /// The agent an ask without a name goes to, once the plugin said.
    pub(crate) fn default_agent(&self) -> Option<&str> {
        let agents = self.agents.as_ref()?;
        (agents.iter().find(|a| a.default))
            .or_else(|| agents.first())
            .map(|a| a.name.as_str())
    }

    /// The agent `text`, a comment the user just wrote, names with its
    /// first word, as `@codex` in `@codex is this safe?`. Until the plugin
    /// listed its agents any name counts, and the plugin says whether it's
    /// one; after, only theirs do, the others naming people.
    pub(crate) fn mentioned(&self, text: &str) -> Option<String> {
        let rest = text.trim_start().strip_prefix('@')?;
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        let name = rest[..end].to_ascii_lowercase();
        self.is_agent(&name).then_some(name)
    }

    /// Whether `name` is an agent's: one the plugin listed, or until it
    /// listed them, any.
    fn is_agent(&self, name: &str) -> bool {
        let listed = |agents: &[Agent]| agents.iter().any(|a| a.name == name);
        !name.is_empty() && self.agents.as_deref().is_none_or(listed)
    }

    /// Whether `name` is one of the agents the plugin listed: none until it
    /// has, as any name may be a person's or an MCP client's.
    pub(crate) fn lists(&self, name: &str) -> bool {
        (self.agents.iter().flatten()).any(|a| a.name == name)
    }

    /// Whether the thread with first comment `root` waits on an agent.
    pub(crate) fn asking(&self, root: &str) -> bool {
        self.asks.iter().any(|a| a.root == root)
    }

    /// Asks `agent`, or the default one, about the thread with first
    /// comment `root`, `prompt` being the thread as a prompt. `mention` is
    /// the comment whose `@<name>` asks, written kept local.
    pub(crate) fn ask(
        &mut self,
        root: String,
        prompt: String,
        (agent, mention): (Option<String>, Option<String>),
    ) -> Vec<Effect> {
        if let Some(ask) = self.asks.iter().find(|a| a.root == root) {
            return vec![Effect::Say(format!(
                "already asking {} about this thread",
                ask.name()
            ))];
        }
        self.run_id += 1;
        let id = self.run_id;
        self.asks.push(Ask {
            id,
            root,
            agent: None,
            progress: None,
            mention,
        });
        vec![
            Effect::Agent {
                id,
                request: Request::Ask { agent, prompt },
            },
            Effect::Redraw,
        ]
    }

    /// The plugin initialized, listing these agents, or couldn't.
    pub(crate) fn listed(&mut self, agents: Result<Vec<Agent>, String>) -> Vec<Effect> {
        self.agents = agents.ok();
        vec![Effect::Redraw]
    }

    /// What the plugin said of ask `id`. An answer goes in the thread as
    /// the agent's reply, through `comments`; an ask no longer wanted is
    /// ignored.
    pub(crate) fn said(
        &mut self,
        (id, said): (u64, Said),
        mut comments: Option<&mut Comments>,
    ) -> Vec<Effect> {
        let Some(at) = self.asks.iter().position(|a| a.id == id) else {
            return Vec::new();
        };
        match said {
            Said::Asking(agent) => self.asks[at].agent = Some(agent),
            Said::Progress(progress) => self.asks[at].progress = Some(progress),
            Said::Answered(answer) => {
                let mut ask = self.asks.remove(at);
                let mut out = Vec::new();
                // No agent took it: the name may be a person's.
                if let (Some(id), None) = (ask.mention.take(), &ask.agent) {
                    out = draft_again(&id, comments.as_deref_mut());
                }
                out.extend(answered(&ask, answer, comments));
                out.push(Effect::Redraw);
                return out;
            }
            Said::NotInstalled => {
                let ask = self.asks.remove(at);
                if let Some(id) = &ask.mention {
                    let mut out = draft_again(id, comments);
                    out.push(Effect::Redraw);
                    return out;
                }
                return vec![
                    Effect::Fail(format!(
                        "{} isn't on PATH (see docs/plugins.md)",
                        agent::PROGRAM
                    )),
                    Effect::Redraw,
                ];
            }
            Said::Checks(_) => {}
        }
        vec![Effect::Redraw]
    }

    /// Stops the ask on the thread with first comment `root`, or with
    /// `None` every ask: the plugin is told, and what it says of them
    /// from now on is ignored.
    pub(crate) fn stop(&mut self, root: Option<&str>) -> Vec<Effect> {
        let (stopped, kept): (Vec<Ask>, Vec<Ask>) = std::mem::take(&mut self.asks)
            .into_iter()
            .partition(|a| root.is_none_or(|r| a.root == r));
        self.asks = kept;
        let said = match stopped.as_slice() {
            [] => return Vec::new(),
            [one] => format!("stopped asking {}", one.name()),
            many => format!("stopped {}", plural(many.len(), "ask")),
        };
        vec![
            Effect::StopAgents(stopped.iter().map(|a| a.id).collect()),
            Effect::Say(said),
            Effect::Redraw,
        ]
    }

    /// Whether any ask runs.
    pub(crate) fn busy(&self) -> bool {
        !self.asks.is_empty()
    }

    /// The status bar's words while asks run: the first one's, as
    /// `asking claude… thinking…`, and how many more. Leaves out the ask on the thread with first comment `shown`, whose
    /// words are already on screen.
    pub(crate) fn status_besides(&self, shown: Option<&str>) -> Option<String> {
        let mut others = self.asks.iter().filter(|a| Some(a.root.as_str()) != shown);
        let mut words = others.next()?.words();
        let more = others.count();
        if more > 0 {
            words.push_str(" · ");
            words.push_str(&plural(more, "more ask"));
        }
        Some(words)
    }

    /// The words of the ask on the thread with first comment `root`, while
    /// it runs.
    pub(crate) fn status_of(&self, root: &str) -> Option<String> {
        Some(self.asks.iter().find(|a| a.root == root)?.words())
    }

    /// The first ask, as work for the program status: `Asking claude`.
    pub(crate) fn work(&self) -> Option<String> {
        let first = self.asks.first()?;
        Some(format!("Asking {}", first.name()))
    }
}

/// The mention `id`, which no agent took, is a draft again.
fn draft_again(id: &str, comments: Option<&mut Comments>) -> Vec<Effect> {
    let Some(comments) = comments else {
        return Vec::new();
    };
    let op = Op::Set {
        target: Target::Id(id.to_owned()),
        change: Box::new(Change {
            author: Some(String::new()),
            ..Change::default()
        }),
    };
    match comments.apply(vec![op]) {
        Ok((written, _)) => read_back(written),
        Err(e) => vec![Effect::Fail(format!("comments: {e}"))],
    }
}

/// What came of `ask`: the answer as the agent's reply in its thread,
/// marked as the plugin marks the agent's replies, or why there's none.
fn answered(
    ask: &Ask,
    answer: Result<Answer, String>,
    comments: Option<&mut Comments>,
) -> Vec<Effect> {
    let name = ask.name();
    let answer = match answer {
        Ok(answer) if answer.text.trim().is_empty() && answer.suggestion.is_none() => {
            return vec![Effect::Fail(format!("{name} answered nothing"))];
        }
        Ok(answer) => answer,
        Err(why) => return vec![Effect::Fail(why)],
    };
    let Some(comments) = comments else {
        return vec![Effect::Fail(format!(
            "{name}'s answer has no comments file to go in"
        ))];
    };
    let reply = Comment {
        text: answer.text.trim().to_owned(),
        suggestion: answer.suggestion,
        author: name.to_owned(),
        label: ask.agent.as_ref().and_then(|a| a.mark.clone()),
        ..Comment::default()
    };
    let op = Op::Add {
        comment: Box::new(reply),
        reply_to: Some(Target::Id(ask.root.clone())),
    };
    match comments.apply(vec![op]) {
        Ok((written, _)) => {
            let mut out = read_back(written);
            out.push(Effect::Say(format!("{name} answered")));
            out
        }
        Err(e) => vec![Effect::Fail(format!("{name}'s answer isn't kept: {e}"))],
    }
}

#[cfg(test)]
mod tests;
