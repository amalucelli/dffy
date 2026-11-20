//! Renderer plugins (experimental): programs named `dffy-render-<name>`
//! that turn both sides of a file into blocks of styled lines, which the
//! viewer then diffs instead of the raw lines. The config's `render` key
//! maps file extensions to renderer names; nothing is looked up or run
//! until the viewer asks for a rendered file.
//!
//! The protocol is one JSON request on the renderer's stdin and one JSON
//! answer on its stdout, both carrying [`PROTOCOL`]; docs/plugins.md has
//! the shapes. A renderer runs in its own process group with no stderr,
//! is killed whole on timeout, on cancellation and when dffy exits, its
//! group is killed once it exits, and its answer is checked against the
//! sides before anything uses it.
//!
//! Review plugins are in [`review`] and the agent plugin in [`agent`], each
//! over the long-lived JSON-RPC process of [`rpc`].

pub(crate) mod agent;
pub(crate) mod json;
pub(crate) mod review;
pub(crate) mod rpc;

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::ops::Range;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError, mpsc};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rustix::process::{Pid, WaitId, WaitIdOptions};

use dffy_core::cache::{self, Cache};
use dffy_core::view::{Attributes, Paint, Syntax};

use json::Value;

/// The protocol version: in every request and every answer. An answer of
/// another version is refused.
pub(crate) const PROTOCOL: usize = 1;

/// How long a renderer may take before the line diff stays.
pub(crate) const TIMEOUT: Duration = Duration::from_millis(500);

/// Answers longer than this are refused rather than read whole.
const MAX_ANSWER: u64 = 64 << 20;

/// How often a running renderer is checked for cancellation.
const POLL: Duration = Duration::from_millis(5);

/// Renderer executables are named this, then the renderer's name.
pub(crate) const PREFIX: &str = "dffy-render-";

/// Review plugin executables are named this, then the plugin's name (see
/// [`review`]).
pub(crate) const REVIEW_PREFIX: &str = "dffy-review-";

/// Checks a plugin's name: lowercase letters, digits, `-` and `_`, so a
/// setting can only ever name a program on PATH with the plugin's prefix.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
    {
        return Err(format!(
            "'{name}' is not a plugin name: lowercase letters, digits, - and _"
        ));
    }
    Ok(())
}

/// The `render` setting: which renderer each file extension gets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Renderers(Vec<(String, String)>);

impl Renderers {
    /// Parses `md:markdown csv:csv`: space-separated `extension:name`
    /// pairs, each name as [`check_name`] takes it.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let mut pairs = Vec::new();
        for pair in value.split_whitespace() {
            let (ext, name) = pair
                .split_once(':')
                .ok_or_else(|| format!("'{pair}' is not extension:renderer, as in md:markdown"))?;
            let ext = ext.strip_prefix('.').unwrap_or(ext);
            if ext.is_empty()
                || !ext
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
            {
                return Err(format!("'{ext}' is not a file extension"));
            }
            check_name(name)?;
            pairs.push((ext.to_ascii_lowercase(), name.to_owned()));
        }
        Ok(Self(pairs))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `(extension, name)` pairs as configured.
    pub(crate) fn pairs(&self) -> &[(String, String)] {
        &self.0
    }

    /// The renderer for the file at `path`, by its extension.
    pub(crate) fn for_path(&self, path: &[u8]) -> Option<&str> {
        let ext = extension(path)?;
        self.0
            .iter()
            .find(|(e, _)| e.as_bytes().eq_ignore_ascii_case(ext))
            .map(|(_, name)| name.as_str())
    }
}

impl std::fmt::Display for Renderers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, (ext, name)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{ext}:{name}")?;
        }
        Ok(())
    }
}

/// The extension of the file at `path`, without its dot: what follows the
/// last dot of the file name, unless that dot starts the name.
pub(crate) fn extension(path: &[u8]) -> Option<&[u8]> {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let dot = name.iter().rposition(|&b| b == b'.').filter(|&i| i > 0)?;
    Some(&name[dot + 1..]).filter(|ext| !ext.is_empty())
}

/// An installed plugin: its executable, and a stamp of it (modification
/// time and size) that changes when it's replaced, for the disk cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Program {
    pub name: String,
    pub path: PathBuf,
    stamp: (u128, u64),
}

/// The first executable `<prefix><name>` in an absolute directory of
/// `PATH`. Empty and relative entries are skipped: they'd resolve inside
/// the repository being viewed, which could then supply the program.
pub(crate) fn find(prefix: &str, name: &str) -> Option<Program> {
    let file = format!("{prefix}{name}");
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
        if !dir.is_absolute() {
            return None;
        }
        let path = dir.join(&file);
        let meta = std::fs::metadata(&path).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then(|| {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            Program {
                name: name.to_owned(),
                path,
                stamp: (modified, meta.len()),
            }
        })
    })
}

/// What a renderer is asked to draw. `None` for a side that doesn't exist
/// (an added or deleted file).
pub(crate) struct Request<'a> {
    pub path: &'a str,
    pub old: Option<&'a str>,
    pub new: Option<&'a str>,
    /// Columns of a pane's text.
    pub width: usize,
    /// The terminal's background is light.
    pub light: bool,
}

impl Request<'_> {
    pub(crate) fn encode(&self) -> String {
        let mut out = String::with_capacity(
            64 + self.path.len()
                + self.old.map_or(0, |s| s.len() + s.len() / 8)
                + self.new.map_or(0, |s| s.len() + s.len() / 8),
        );
        let _ = write!(out, "{{\"version\":{PROTOCOL},\"path\":");
        json::string(&mut out, self.path);
        for (key, side) in [("old", self.old), ("new", self.new)] {
            let _ = write!(out, ",\"{key}\":");
            match side {
                Some(text) => json::string(&mut out, text),
                None => out.push_str("null"),
            }
        }
        let background = if self.light { "light" } else { "dark" };
        let _ = writeln!(
            out,
            ",\"width\":{},\"background\":\"{background}\"}}",
            self.width
        );
        out
    }

    /// The disk cache's key: the renderer as installed, and everything the
    /// request holds.
    fn cache_key(&self, program: &Program) -> Vec<u8> {
        let mut key = Vec::with_capacity(128);
        key.extend_from_slice(&PROTOCOL.to_le_bytes());
        key.extend_from_slice(program.name.as_bytes());
        key.push(0);
        key.extend_from_slice(&program.stamp.0.to_le_bytes());
        key.extend_from_slice(&program.stamp.1.to_le_bytes());
        key.extend_from_slice(self.path.as_bytes());
        key.push(0);
        for side in [self.old, self.new] {
            match side {
                Some(text) => {
                    key.push(1);
                    key.extend_from_slice(cache::digest(text.as_bytes()).as_bytes());
                }
                None => key.push(0),
            }
        }
        key.extend_from_slice(&self.width.to_le_bytes());
        key.push(u8::from(self.light));
        key
    }
}

/// One side as a renderer drew it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Side {
    pub blocks: Vec<Block>,
}

/// A piece of a rendered document that changes only whole: a heading, a
/// paragraph, a table row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Block {
    /// The renderer's word for it (`heading`, `paragraph`, `code`); part
    /// of its identity.
    pub kind: String,
    /// The lines of the side's source it was drawn from, 0-based.
    pub source: Range<usize>,
    pub lines: Vec<Line>,
    /// For a code block, the language its fence named.
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub text: String,
    /// In order and not overlapping.
    pub paint: Vec<Paint>,
    /// The bytes of each span that links somewhere, with its target: not
    /// drawn, but a changed target is a change.
    pub links: Vec<(Range<usize>, String)>,
}

/// Why a file shows its line diff instead of a rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Spawn(String),
    Timeout,
    Exited(String),
    /// The renderer's own message.
    Said(String),
    Version(String),
    Malformed(String),
    Cancelled,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "couldn't start: {e}"),
            Self::Timeout => write!(f, "took over {}ms", TIMEOUT.as_millis()),
            Self::Exited(status) => write!(f, "{status}"),
            Self::Said(message) => write!(f, "said: {message}"),
            Self::Version(v) => write!(f, "speaks protocol {v}, dffy speaks {PROTOCOL}"),
            Self::Malformed(e) => write!(f, "bad answer: {e}"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Renders `request` with `program`, through `cache` when there is one.
/// `lines` is each side's line count, which block sources must fit; a side
/// the request doesn't have is ignored in the answer.
/// `cancelled` is polled while the renderer runs; once it says so, the
/// renderer is killed. Answers are only cached once they've checked out.
pub(crate) fn render(
    program: &Program,
    request: &Request,
    lines: [usize; 2],
    cache: Option<&Cache>,
    cancelled: impl Fn() -> bool,
) -> Result<[Side; 2], Failure> {
    let lines = [request.old.map(|_| lines[0]), request.new.map(|_| lines[1])];
    let key = cache.map(|_| request.cache_key(program));
    if let (Some(cache), Some(key)) = (cache, &key)
        && let Some(answer) = cache.get(cache::Kind::Rendered, key)
        && let Ok(sides) = decode(&answer, lines)
    {
        return Ok(sides);
    }
    let mut command = Command::new(&program.path);
    command.process_group(0);
    let (answer, status) = run(command, request.encode().into_bytes(), TIMEOUT, cancelled)?;
    match (decode(&answer, lines), status.success()) {
        // The renderer's own message says more than how it exited.
        (Err(said @ Failure::Said(_)), _) => Err(said),
        (Err(_) | Ok(_), false) => Err(Failure::Exited(exited(status))),
        (Err(failure), true) => Err(failure),
        (Ok(sides), true) => {
            if let (Some(cache), Some(key)) = (cache, &key) {
                cache.put(cache::Kind::Rendered, key, &answer);
            }
            Ok(sides)
        }
    }
}

/// The process groups of the renderers running now, and whether dffy is
/// exiting, after which a renderer is killed as soon as it starts.
struct Running {
    groups: Vec<Pid>,
    stopped: bool,
}

static RUNNING: Mutex<Running> = Mutex::new(Running {
    groups: Vec::new(),
    stopped: false,
});

fn running() -> MutexGuard<'static, Running> {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

fn kill_group(leader: Pid) {
    rustix::process::kill_process_group(leader, rustix::process::Signal::KILL).ok();
}

/// Kills every renderer still running, with everything it started, and
/// any started from now on: for dffy's way out, whichever it takes. The
/// worker threads running them don't outlive the process to do it.
pub(crate) fn stop_all() {
    stop(&mut running());
}

fn stop(running: &mut Running) {
    running.stopped = true;
    for &leader in &running.groups {
        kill_group(leader);
    }
}

/// What a panic runs before it kills what's running: each a plugin asked
/// to stop on its own (see [`rpc::Client::shut_down_on_panic`]).
type OnPanic = Box<dyn Fn() + Send>;

static ON_PANIC: Mutex<Vec<OnPanic>> = Mutex::new(Vec::new());

fn on_panic(run: impl Fn() + Send + 'static) {
    ON_PANIC
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Box::new(run));
}

/// Makes a panic run [`stop_all`] before the hook installed before, unless
/// another thread holds the list of renderers then. Plugins that asked to
/// are asked to stop first, unless another thread holds their list.
pub(crate) fn stop_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(runs) = ON_PANIC.try_lock() {
            runs.iter().for_each(|run| run());
        }
        match RUNNING.try_lock() {
            Ok(mut running) => stop(&mut running),
            Err(TryLockError::Poisoned(e)) => stop(&mut e.into_inner()),
            Err(TryLockError::WouldBlock) => {}
        }
        previous(info);
    }));
}

/// Kills the renderer and everything it started, then reaps it.
fn kill(child: &mut Child) {
    let leader = Pid::from_child(child);
    {
        let mut running = running();
        running.groups.retain(|&g| g != leader);
        kill_group(leader);
    }
    child.kill().ok();
    child.wait().ok();
}

/// Reaps the renderer if it has exited. Its group leaves the list in the
/// same step, so [`stop_all`] never signals a reused id.
fn reap(child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
    let mut running = running();
    let status = child.try_wait()?;
    if status.is_some() {
        let leader = Pid::from_child(child);
        running.groups.retain(|&g| g != leader);
    }
    Ok(status)
}

/// Runs `command`, a renderer, on `input` within `timeout`, and returns
/// what it printed and how it exited. `command` puts the renderer in a
/// process group of its own, through `process_group`, which is killed with
/// it.
fn run(
    mut command: Command,
    input: Vec<u8>,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<(Vec<u8>, ExitStatus), Failure> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Its messages would land on the viewer's screen.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Failure::Spawn(e.to_string()))?;
    {
        let mut running = running();
        if running.stopped {
            drop(running);
            kill(&mut child);
            return Err(Failure::Cancelled);
        }
        running.groups.push(Pid::from_child(&child));
    }
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        kill(&mut child);
        return Err(Failure::Spawn("no pipes".to_owned()));
    };
    let (tx, rx) = mpsc::channel();
    // Written from a thread of its own, so a renderer answering before it
    // has read everything can't deadlock against us. A renderer that
    // doesn't read at all only costs a broken pipe here.
    let threads = std::thread::Builder::new()
        .name("dffy-render-in".into())
        .spawn(move || stdin.write_all(&input))
        .and_then(|_| {
            std::thread::Builder::new()
                .name("dffy-render-out".into())
                .spawn(move || {
                    let mut out = Vec::new();
                    let read = stdout.take(MAX_ANSWER + 1).read_to_end(&mut out);
                    tx.send(read.map(|_| out)).ok();
                })
        });
    if let Err(e) = threads {
        kill(&mut child);
        return Err(Failure::Spawn(e.to_string()));
    }
    // Neither thread is ever joined: a grandchild holding the pipes open
    // past a kill would hold us too.
    let out = loop {
        match rx.recv_timeout(POLL) {
            Ok(Ok(out)) => break out,
            Ok(Err(e)) => {
                kill(&mut child);
                return Err(Failure::Malformed(format!("reading it failed: {e}")));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                kill(&mut child);
                return Err(Failure::Malformed("its output went away".to_owned()));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if cancelled() => {
                kill(&mut child);
                return Err(Failure::Cancelled);
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                kill(&mut child);
                return Err(Failure::Timeout);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    if out.len() as u64 > MAX_ANSWER {
        kill(&mut child);
        return Err(Failure::Malformed(format!("over {}MB", MAX_ANSWER >> 20)));
    }
    // Its output is closed; it should be exiting. Once it has, what it
    // started is killed with its group: until it's reaped, its id, the
    // group's, can't be reused.
    let leader = Pid::from_child(&child);
    let exited = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    loop {
        match rustix::process::waitid(WaitId::Pid(leader), exited) {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                kill(&mut child);
                return Err(Failure::Timeout);
            }
        }
    }
    kill_group(leader);
    let Ok(Some(status)) = reap(&mut child) else {
        kill(&mut child);
        return Err(Failure::Malformed(
            "its exit status went missing".to_owned(),
        ));
    };
    Ok((out, status))
}

/// How a plugin that failed exited, as words.
fn exited(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with status {code}"),
        None => "was killed by a signal".to_owned(),
    }
}

/// Reads an answer, checking it against the request, whose sides had
/// `lines` lines (`None` for a side it didn't have): its version, blocks
/// for every side sent, each block's source within its side's lines, and
/// no line holding a line break.
pub(crate) fn decode(bytes: &[u8], lines: [Option<usize>; 2]) -> Result<[Side; 2], Failure> {
    let value = json::parse(bytes).map_err(Failure::Malformed)?;
    if !matches!(value, Value::Object(_)) {
        return Err(Failure::Malformed("not a JSON object".to_owned()));
    }
    match value.get("version").and_then(Value::as_usize) {
        Some(PROTOCOL) => {}
        Some(other) => return Err(Failure::Version(other.to_string())),
        None => return Err(Failure::Malformed("no version".to_owned())),
    }
    if let Some(error) = value.get("error").filter(|e| **e != Value::Null) {
        let message = error.as_str().unwrap_or("an error that isn't text");
        return Err(Failure::Said(message.to_owned()));
    }
    let side = |key: &str, lines: Option<usize>| -> Result<Side, Failure> {
        let Some(lines) = lines else {
            return Ok(Side::default());
        };
        match value.get(key) {
            None | Some(Value::Null) => Err(Failure::Malformed(format!("no blocks for {key}"))),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    block(b, lines).map_err(|e| Failure::Malformed(format!("{key} block {i}: {e}")))
                })
                .collect::<Result<_, _>>()
                .map(|blocks| Side { blocks }),
            Some(_) => Err(Failure::Malformed(format!("{key} isn't a list of blocks"))),
        }
    };
    Ok([side("old", lines[0])?, side("new", lines[1])?])
}

fn block(value: &Value, side_lines: usize) -> Result<Block, String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("no kind")?
        .to_owned();
    let source = match value.get("source").and_then(Value::as_array) {
        Some([start, end]) => {
            let (start, end) = start
                .as_usize()
                .zip(end.as_usize())
                .ok_or("source isn't two line numbers")?;
            if start > end || end > side_lines {
                return Err(format!(
                    "source {start}..{end} is outside the side's {side_lines} lines"
                ));
            }
            start..end
        }
        _ => return Err("source isn't [start, end]".to_owned()),
    };
    let lines = value
        .get("lines")
        .and_then(Value::as_array)
        .ok_or("lines isn't a list")?
        .iter()
        .map(line)
        .collect::<Result<_, _>>()?;
    let language = value
        .get("language")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Block {
        kind,
        source,
        lines,
        language,
    })
}

/// A line: a list of spans, each a string or `{"text", "style", ...}`.
fn line(value: &Value) -> Result<Line, String> {
    let spans = value.as_array().ok_or("a line isn't a list of spans")?;
    let mut text = String::new();
    let mut paint = Vec::new();
    let mut links = Vec::new();
    for span in spans {
        let (piece, style) = match span {
            Value::String(s) => (s.as_str(), None),
            Value::Object(_) => (
                span.get("text")
                    .and_then(Value::as_str)
                    .ok_or("a span has no text")?,
                Some(span),
            ),
            _ => return Err("a span isn't text or an object".to_owned()),
        };
        if piece.contains(['\n', '\r']) {
            return Err("a span holds a line break; lines are separate lists".to_owned());
        }
        let start = text.len();
        text.push_str(piece);
        if let Some(url) = style.and_then(|s| s.get("url")).and_then(Value::as_str) {
            links.push((start..text.len(), url.to_owned()));
        }
        if let Some((syntax, attributes)) = style
            .map(style_of)
            .filter(|s| *s != (None, Attributes::NONE))
        {
            let offset = |n: usize| u32::try_from(n).map_err(|_| "a line over 4GB".to_owned());
            paint.push(Paint {
                start: offset(start)?,
                end: offset(text.len())?,
                syntax,
                attributes,
            });
        }
    }
    Ok(Line { text, paint, links })
}

/// What a span's `style` names come to. Unknown names are ignored, so
/// renderers can use names a later dffy understands.
fn style_of(span: &Value) -> (Option<Syntax>, Attributes) {
    let (mut syntax, mut attributes) = (None, Attributes::NONE);
    let names = span.get("style").and_then(Value::as_array).unwrap_or(&[]);
    for name in names.iter().filter_map(Value::as_str) {
        match name {
            "bold" => attributes |= Attributes::BOLD,
            "italic" => attributes |= Attributes::ITALIC,
            "underline" => attributes |= Attributes::UNDERLINE,
            "dim" => attributes |= Attributes::DIM,
            "code" => syntax = Some(Syntax::String),
            "link" => {
                syntax = syntax.or(Some(Syntax::Function));
                attributes |= Attributes::UNDERLINE;
            }
            "heading" => {
                let level = span.get("level").and_then(Value::as_usize).unwrap_or(1);
                attributes |= Attributes::BOLD;
                if level <= 2 {
                    syntax = Some(Syntax::Keyword);
                }
                if level == 1 {
                    attributes |= Attributes::UNDERLINE;
                }
            }
            other => {
                if let Some(s) = other
                    .strip_prefix("syntax-")
                    .and_then(|key| Syntax::ALL.into_iter().find(|s| s.key() == key))
                {
                    syntax = Some(s);
                }
            }
        }
    }
    (syntax, attributes)
}

#[cfg(test)]
mod tests;
