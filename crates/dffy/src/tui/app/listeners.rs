//! The coding agents' sessions listening for threads in this view's working
//! tree: which one a hand-over goes to, the list to pick from when that
//! isn't clear, and the question before sending every open thread.
//!
//! A session asks again after each hand-over it gets, from any view, so for
//! a moment nobody holds its request. It counts as listening through that
//! moment, for at most [`GAP`], and what's handed to it meanwhile waits for
//! it to ask.

use std::cmp::Reverse;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{Action, App, Confirmed, Handed, Handoff, To, escape};
use crate::control::viewer::{Facts, Listens, Reply};
use crate::keys::Action as Act;
use crate::review::model::text::{ago, now};
use crate::review::session::plural;
use crate::term::{Grid, Input, Key, Mods};
use crate::theme::{Role, Style, Theme};
use crate::tui::help;
use crate::tui::menu::Run;

/// How long a session that isn't asking still counts as listening: dffyd
/// calls a request off the same way whether another view answered it or its
/// session left.
const GAP: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct Listeners {
    /// In the order they came.
    known: Vec<Listener>,
    /// The session picked, by its number, which no other session gets from
    /// the same dffyd.
    chosen: Option<u64>,
    /// Those that wait for their session to ask again, the first first.
    waiting: Vec<Handoff>,
    pub(super) picking: Option<Picker>,
    /// Some session listens: worked out as they change, for each frame.
    pub(super) any: bool,
}

struct Listener {
    number: u64,
    request: u64,
    client: Option<String>,
    facts: Facts,
    state: State,
}

enum State {
    /// Its request waits for an answer.
    Asking(Reply),
    /// Its request is being answered, on a thread that holds the reply.
    Taken(Weak<AtomicBool>),
    /// Its request was answered, or called off, then.
    Between(Instant),
}

/// Where a hand-over goes.
pub(crate) enum HandTo {
    /// No session listens: a new agent.
    Agent,
    /// A session, through its reply.
    Session(Reply, Listening),
    /// Nowhere yet: the app keeps it, and gives it again if it's to go.
    Later,
}

/// A session threads are handed to, in words safe to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listening {
    pub number: u64,
    pub name: String,
    /// ` · edited 3h ago`, when others listen too.
    pub told_apart: String,
}

impl Listening {
    fn shown(&self) -> String {
        format!("{}{}", self.name, self.told_apart)
    }
}

enum Target {
    Nobody,
    One(Listening),
    Unclear,
}

impl Listener {
    /// Its name, safe to draw.
    fn name(&self) -> String {
        (self.client.as_deref()).map_or_else(|| "a session".to_owned(), |c| escape(c.as_bytes()))
    }

    /// When it stops counting as listening.
    fn until(&self) -> Option<Instant> {
        match self.state {
            State::Between(at) => Some(at + GAP),
            State::Asking(_) | State::Taken(_) => None,
        }
    }

    fn live(&self) -> bool {
        self.until().is_none_or(|at| Instant::now() < at)
    }

    /// ` · edited 3h ago`, or prompted; `None` when it did neither.
    fn last_did(&self) -> Option<String> {
        let edited = self.facts.edited.map(|at| did("edited", at));
        edited.or_else(|| self.facts.prompted.map(|at| did("prompted", at)))
    }

    fn started(&self) -> String {
        did("started", self.facts.started)
    }

    /// Its row in the list: `claude · started 2h ago · edited 3m ago`.
    fn row(&self) -> String {
        let last = self.last_did().unwrap_or_default();
        format!("{}{}{last}", self.name(), self.started())
    }
}

/// ` · edited 3h ago`.
fn did(what: &str, at: u64) -> String {
    let ago = ago(now().saturating_sub(at)).unwrap_or_else(|| "a while ago".to_owned());
    format!(" · {what} {ago}")
}

impl Listeners {
    fn live(&self) -> impl Iterator<Item = &Listener> {
        self.known.iter().filter(|l| l.live())
    }

    fn number(&self, number: u64) -> Option<&Listener> {
        self.known.iter().find(|l| l.number == number)
    }

    fn target(&self) -> Target {
        let live: Vec<&Listener> = self.live().collect();
        let mut edited = live.iter().filter(|l| l.facts.edited.is_some());
        let chosen = live.iter().find(|l| Some(l.number) == self.chosen);
        match (&live[..], chosen, (edited.next(), edited.next())) {
            ([], ..) => Target::Nobody,
            ([one], ..) | (_, Some(one), _) | (_, _, (Some(one), None)) => {
                let told_apart = if live.len() > 1 {
                    one.last_did().unwrap_or_else(|| one.started())
                } else {
                    String::new()
                };
                Target::One(Listening {
                    number: one.number,
                    name: one.name(),
                    told_apart,
                })
            }
            _ => Target::Unclear,
        }
    }

    /// Those listening, the most likely one first.
    fn ranked(&self) -> Vec<&Listener> {
        let mut live: Vec<&Listener> = self.live().collect();
        live.sort_by_key(|l| {
            let Facts {
                started,
                edited,
                prompted,
            } = l.facts;
            Reverse((edited, prompted, started, l.number))
        });
        live
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.known.iter().filter_map(Listener::until).min()
    }
}

impl App {
    /// `listens` asks, for the first time or again.
    pub(super) fn listens(&mut self, listens: Listens, reply: Reply) {
        let Listens {
            request,
            listener: number,
            client,
            facts,
        } = listens;
        let listener = Listener {
            number,
            request,
            client,
            facts,
            state: State::Asking(reply),
        };
        let known = &mut self.listeners.known;
        match known.iter_mut().find(|l| l.number == number) {
            Some(known) => *known = listener,
            None => known.push(listener),
        }
        self.listeners_changed();
    }

    pub(super) fn listened(&mut self, request: u64, facts: Facts) {
        let mut known = self.listeners.known.iter_mut();
        if let Some(listener) = known.find(|l| l.request == request) {
            listener.facts = facts;
            self.listeners_changed();
        }
    }

    /// Takes the requests called off, and those answered since.
    pub(super) fn listeners_settled(&mut self) {
        let mut changed = false;
        for listener in &mut self.listeners.known {
            let over = match &listener.state {
                State::Asking(reply) => reply.cancelled(),
                State::Taken(flag) => (flag.upgrade()).is_none_or(|f| f.load(Ordering::Relaxed)),
                State::Between(_) => false,
            };
            if over {
                listener.state = State::Between(Instant::now());
                changed = true;
            }
        }
        if changed {
            self.listeners_changed();
        }
    }

    /// dffyd is gone, and the sessions' numbers with it.
    pub(super) fn listeners_gone(&mut self) {
        self.listeners.known.clear();
        self.listeners.chosen = None;
        self.listeners_changed();
    }

    /// After each event: what waited for a session goes once it asks and
    /// the loop took the hand-over before it, or not at all once the session
    /// is gone.
    pub(super) fn listeners_settle(&mut self) {
        let listeners = &mut self.listeners;
        if listeners.known.is_empty() && listeners.waiting.is_empty() {
            return;
        }
        let before = listeners.known.len();
        listeners.known.retain(Listener::live);
        let gone = listeners.known.len() != before;
        let mut lost = None;
        let known = &listeners.known;
        listeners.waiting.retain(|handoff| match &handoff.to {
            To::Session { number, name } if !known.iter().any(|l| l.number == *number) => {
                lost = Some(name.clone());
                false
            }
            _ => true,
        });
        if self.handoff.is_none() {
            let asks = |handoff: &Handoff| match handoff.to {
                To::Session { number, .. } => known
                    .iter()
                    .any(|l| l.number == number && matches!(l.state, State::Asking(_))),
                To::Anyone | To::NewAgent => true,
            };
            if let Some(at) = listeners.waiting.iter().position(asks) {
                self.handoff = Some(listeners.waiting.remove(at));
            }
        }
        if let Some(name) = lost {
            self.failed(stopped(&name));
        }
        if gone {
            self.listeners_changed();
        }
    }

    /// What's shown of the sessions follows them.
    fn listeners_changed(&mut self) {
        let any = self.listeners.live().next().is_some();
        self.dirty |= std::mem::replace(&mut self.listeners.any, any) != any;
        if self.commands.is_some() {
            let (title, entries) = self.command_entries();
            if let Some(commands) = &mut self.commands {
                commands.update(title, entries);
            }
            self.dirty = true;
        }
        if let Some(mut menu) = self.menu.take() {
            menu.relabel(|run| match run {
                Run::Action(act) | Run::Thread(act)
                    if matches!(act, Act::FixWithAgent | Act::SendOpenThreads) =>
                {
                    Some(self.hand_over_title(act))
                }
                _ => None,
            });
            self.menu = Some(menu);
            self.dirty = true;
        }
        let Some(mut picker) = self.listeners.picking.take() else {
            return;
        };
        self.dirty = true;
        // Rows stay where they are: `enter` may be on its way down.
        let ranked = self.listeners.ranked();
        let at = picker.rows.get(picker.cursor).map(|(number, _)| *number);
        picker
            .rows
            .retain(|(number, _)| ranked.iter().any(|l| l.number == *number));
        for listener in ranked {
            let row = listener.row();
            match picker.rows.iter_mut().find(|(n, _)| *n == listener.number) {
                Some((_, shown)) => *shown = row,
                None => picker.rows.push((listener.number, row)),
            }
        }
        if picker.rows.is_empty() {
            self.failed("the sessions stopped listening; send again".to_owned());
            return;
        }
        let kept = picker.rows.iter().position(|(n, _)| Some(*n) == at);
        picker.cursor = kept.unwrap_or_else(|| picker.cursor.min(picker.rows.len() - 1));
        self.listeners.picking = Some(picker);
    }

    /// Where `handoff` goes, opening the list to pick from when that isn't
    /// clear.
    pub(crate) fn hand_over(&mut self, handoff: &Handoff) -> HandTo {
        let session = match &handoff.to {
            To::NewAgent => return HandTo::Agent,
            To::Session { number, name } => {
                if self.listeners.number(*number).is_none_or(|l| !l.live()) {
                    self.failed(stopped(name));
                    return HandTo::Later;
                }
                Listening {
                    number: *number,
                    name: name.clone(),
                    told_apart: match self.listeners.target() {
                        Target::One(s) if s.number == *number => s.told_apart,
                        _ => String::new(),
                    },
                }
            }
            To::Anyone => match self.listeners.target() {
                Target::Nobody => return HandTo::Agent,
                Target::Unclear => {
                    let rows = (self.listeners.ranked().iter())
                        .map(|l| (l.number, l.row()))
                        .collect();
                    self.listeners.picking = Some(Picker {
                        handoff: handoff.clone(),
                        rows,
                        cursor: 0,
                        scroll: 0,
                        shown: None,
                    });
                    self.dirty = true;
                    return HandTo::Later;
                }
                Target::One(session) => session,
            },
        };
        let mut known = self.listeners.known.iter_mut();
        if let Some(listener) = known.find(|l| l.number == session.number)
            && let State::Asking(reply) = &listener.state
        {
            let taken = State::Taken(reply.watch());
            if let State::Asking(reply) = std::mem::replace(&mut listener.state, taken) {
                return HandTo::Session(reply, session);
            }
        }
        if self.listeners.waiting.is_empty() {
            self.message(format!("waiting for {}…", session.name));
            self.listeners.waiting.push(handoff.clone().to(&session));
        } else {
            self.message(format!("still sending to {}; wait", session.name));
        }
        HandTo::Later
    }

    /// A hand-over whose session was called away as its checkpoint was
    /// taken: another view handed it something, or it left. It waits for the
    /// session to ask again, before those that came after it.
    pub(super) fn handed_back(&mut self, handoff: Handoff) {
        self.listeners_settled();
        if let To::Session { name, .. } = &handoff.to {
            self.message(format!("waiting for {name}…"));
        }
        self.listeners.waiting.insert(0, handoff);
    }

    /// Who the keys that hand threads over hand them to, when a session
    /// listens.
    pub(super) fn hands_to(&self) -> Option<String> {
        match self.listeners.target() {
            Target::Nobody => None,
            Target::One(session) => Some(session.shown()),
            Target::Unclear => Some("a session".to_owned()),
        }
    }

    /// `action`'s words in the command panel and the menu: where it hands
    /// threads to, when a session listens, else its own.
    pub(super) fn hand_over_title(&self, action: Act) -> String {
        match (action, self.hands_to()) {
            (Act::FixWithAgent, Some(name)) => format!("Send to {name}"),
            (Act::SendOpenThreads, Some(name)) => format!("Send open threads to {name}"),
            _ => action.title().to_owned(),
        }
    }

    /// `confirm_send_open_threads`: asks before `send_open_threads`, naming
    /// where the threads go. The list to pick from is question enough, and
    /// with nothing to send there's none.
    pub(super) fn ask_to_send(&mut self) {
        let Some(count) = self.open_to_send().and_then(|handoff| match handoff.what {
            Handed::Open { count, .. } => Some(count),
            Handed::Thread(_) => None,
        }) else {
            self.send_open_threads();
            return;
        };
        let threads = plural(count, "thread");
        let (to, prompt) = match self.listeners.target() {
            Target::Unclear => {
                self.send_open_threads();
                return;
            }
            Target::Nobody => (
                To::NewAgent,
                format!("start your agent on {threads}? enter starts · esc keeps"),
            ),
            Target::One(session) => (
                To::Session {
                    number: session.number,
                    name: session.name.clone(),
                },
                format!(
                    "send {threads} to {}? enter sends · esc keeps",
                    session.shown()
                ),
            ),
        };
        self.message = None;
        let asked = Confirmed::Send {
            to,
            count,
            frame: self.frame,
        };
        self.confirm = Some((prompt, asked, NOTHING_SENT));
    }

    /// `enter` to [`App::ask_to_send`]'s question, asked of `count` threads
    /// for `to`: sends them, or asks again when what it says isn't so now.
    pub(super) fn send_asked(&mut self, to: &To, count: usize) {
        self.ask_to_send();
        let same = matches!(
            &self.confirm,
            Some((_, Confirmed::Send { to: now, count: n, .. }, _)) if now == to && *n == count
        );
        if same {
            self.confirm = None;
            self.send_open_threads();
            if let Some(handoff) = &mut self.handoff {
                handoff.to = to.clone();
            }
        }
    }

    /// Input while the list of sessions is open.
    pub(super) fn sessions_input(&mut self, input: &Input) -> Option<Action> {
        let picker = self.listeners.picking.as_mut()?;
        let last = picker.rows.len().saturating_sub(1);
        match *input {
            Input::Key(Key::Char('c'), mods) if mods.contains(Mods::CTRL) => {
                return Some(Action::Quit);
            }
            Input::Key(Key::Esc | Key::Char('q'), _) => {
                self.listeners.picking = None;
                self.message(NOTHING_SENT);
            }
            Input::Key(Key::Up | Key::Char('k'), _) => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            Input::Key(Key::Down | Key::Char('j'), _) => {
                picker.cursor = (picker.cursor + 1).min(last);
            }
            Input::Scroll { delta, .. } => {
                picker.cursor = (picker.cursor)
                    .saturating_add_signed(isize::from(delta))
                    .min(last);
            }
            Input::Click { x, y } => {
                picker.cursor = picker.row_at((usize::from(x), usize::from(y)))?;
                self.pick_session();
            }
            Input::Key(Key::Enter, _) => self.pick_session(),
            _ => {}
        }
        None
    }

    /// The hand-over the list is open for goes to the session its cursor is
    /// on, and so does every one after.
    fn pick_session(&mut self) {
        let Some(picker) = self.listeners.picking.take() else {
            return;
        };
        let session = (picker.rows.get(picker.cursor))
            .and_then(|(number, _)| self.listeners.number(*number))
            .map(|l| (l.number, l.name(), l.live()));
        match session {
            Some((number, name, true)) => {
                self.listeners.chosen = Some(number);
                self.handoff = Some(Handoff {
                    to: To::Session { number, name },
                    ..picker.handoff
                });
            }
            Some((_, name, false)) => self.failed(stopped(&name)),
            None => {}
        }
    }
}

/// What's said when a hand-over was asked about and declined.
const NOTHING_SENT: &str = "nothing sent";

/// What's said of threads that didn't reach the session called `name`.
fn stopped(name: &str) -> String {
    format!("{name} stopped listening; send again")
}

/// The sessions to hand something to.
pub(super) struct Picker {
    handoff: Handoff,
    /// Each session's number and row.
    rows: Vec<(u64, String)>,
    cursor: usize,
    scroll: usize,
    /// The first cell and the size of the rows as last drawn, for taps.
    shown: Option<[(usize, usize); 2]>,
}

impl Picker {
    /// The row at screen cell `(x, y)`.
    fn row_at(&self, (x, y): (usize, usize)) -> Option<usize> {
        let [(x0, y0), (width, height)] = self.shown?;
        let row = y.checked_sub(y0).filter(|row| *row < height)?;
        (x0..x0 + width)
            .contains(&x)
            .then_some(self.scroll + row)
            .filter(|row| *row < self.rows.len())
    }

    pub(super) fn draw(&mut self, grid: &mut Grid, theme: &Theme) {
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 20 || gh < 6 {
            return;
        }
        let full = gw < help::FULL_SCREEN_BELOW;
        let width = if full { gw } else { 64.min(gw - 4) };
        let height = (self.rows.len() + 4).min(if full { gh } else { gh - 2 });
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        help::frame(grid, (x0, y0, width, height), " send to ", theme);
        let shown = height - 3;
        self.shown = Some([(x0 + 1, y0 + 1), (width - 2, shown)]);
        self.scroll = self
            .scroll
            .clamp((self.cursor + 1).saturating_sub(shown), self.cursor);
        let (x, right) = (x0 + 2, x0 + width - 3);
        for (i, (_, row)) in (self.rows.iter().enumerate()).skip(self.scroll).take(shown) {
            let y = y0 + 1 + i - self.scroll;
            let mut style = Style::PLAIN;
            if i == self.cursor {
                style = theme.selected(style, true);
                grid.fill(x - 1, y, right + 1, style);
            }
            grid.put_str(x, y, right, row, style);
        }
        let dim = theme.style(Role::Dim);
        help::scrollbar(
            grid,
            (x0 + width - 2, y0 + 1, shown),
            (self.rows.len(), self.scroll),
            dim,
        );
        grid.put_str(
            x,
            y0 + height - 2,
            right,
            "enter sends · j/k move · esc sends nothing",
            dim,
        );
    }
}
