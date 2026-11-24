//! The control API from the outside: a connection to dffyd, finding the
//! view a command means, and `dffy ctl`. Clients never start dffyd: when
//! none runs, no viewer does.

use std::io::{self, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use dffy_core::git::Repo;

use super::{Fault, VERSION, line, number, object, text};
use crate::Error;
use crate::cli::{CtlAction, CtlArgs};
use crate::plugin::json::Value;

/// How long a client waits for an answer. A viewer answers between frames,
/// so only one whose loop is held, as while the editor `e` opened runs, is
/// slower.
const ANSWER: Duration = Duration::from_secs(5);

pub(crate) const NO_VIEWER: &str = "no dffy viewer is running";

/// `dffy ctl`'s name as a client, and MCP's: what the comments they add
/// are by.
pub(crate) const CTL: &str = "dffy ctl";
pub(crate) const MCP: &str = "dffy mcp";

pub(crate) struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
}

/// A connection to the dffyd serving the control API, past the handshake;
/// `None` when none runs.
///
/// # Errors
///
/// When dffyd refuses the handshake or the socket fails.
pub(crate) fn connect(name: &str) -> io::Result<Option<Client>> {
    let path = super::socket_path(&crate::daemon::runtime_path());
    let Ok(stream) = crate::daemon::connect(&path) else {
        return Ok(None);
    };
    stream.set_read_timeout(Some(ANSWER))?;
    let mut client = Client {
        writer: stream.try_clone()?,
        reader: BufReader::new(stream),
        next: 0,
    };
    let hello = object(vec![
        ("version", number(VERSION as u64)),
        ("client", text(name)),
    ]);
    client.writer.write_all(line(&hello).as_bytes())?;
    let welcome = client.receive()?;
    if let Some(error) = welcome.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("refused");
        return Err(io::Error::other(format!("dffyd: {message}")));
    }
    Ok(Some(client))
}

impl Client {
    fn receive(&mut self) -> io::Result<Value> {
        let mut buf = Vec::new();
        match super::read(&mut self.reader, &mut buf)? {
            Some(Ok(value)) => Ok(value),
            Some(Err(e)) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
            None => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }

    /// Asks `method` and waits for its answer; notifications that come
    /// first are skipped.
    pub(crate) fn call(&mut self, method: &str, params: Value) -> Result<Value, Fault> {
        self.next += 1;
        let id = number(self.next);
        let request = object(vec![
            ("id", id.clone()),
            ("method", text(method)),
            ("params", params),
        ]);
        let lost = |e: &io::Error| match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Fault::new(
                "failed",
                format!(
                    "the viewer didn't answer within {}s; it may be waiting for its editor to close",
                    ANSWER.as_secs()
                ),
            ),
            _ => Fault::new("failed", format!("dffyd: {e}")),
        };
        self.writer
            .write_all(line(&request).as_bytes())
            .map_err(|e| lost(&e))?;
        loop {
            let answer = self.receive().map_err(|e| lost(&e))?;
            if answer.get("id") != Some(&id) {
                continue;
            }
            if let Some(error) = answer.get("error") {
                let field = |key: &str| error.get(key).and_then(Value::as_str).unwrap_or_default();
                return Err(Fault::new(super::code(field("code")), field("message")));
            }
            return answer
                .get("result")
                .cloned()
                .ok_or_else(|| Fault::new("failed", "dffyd's answer has no result"));
        }
    }

    /// Asks `view` `method`, its id added to `params`.
    pub(crate) fn ask(
        &mut self,
        view: &View,
        method: &str,
        params: Vec<(&str, Value)>,
    ) -> Result<Value, String> {
        let mut fields = vec![("view", number(view.id))];
        fields.extend(params);
        self.call(method, object(fields)).map_err(|f| f.message)
    }
}

/// A registered view, as `views/list` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct View {
    pub id: u64,
    pub pid: u64,
    pub worktree: PathBuf,
    pub diff: String,
}

impl View {
    fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("view")?.as_usize()? as u64,
            pid: value.get("pid")?.as_usize()? as u64,
            worktree: PathBuf::from(value.get("worktree")?.as_str()?),
            diff: value.get("diff")?.as_str()?.to_owned(),
        })
    }
}

pub(crate) fn views(client: &mut Client) -> Result<Vec<View>, String> {
    let listed = client
        .call("views/list", object(Vec::new()))
        .map_err(|f| f.message)?;
    Ok(listed
        .get("views")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(View::parse)
        .collect())
}

/// The working tree `dir` is in, canonical as viewers register theirs.
pub(crate) fn worktree_of(dir: &Path) -> Option<PathBuf> {
    let repo = Repo::discover(dir).ok()?;
    let root = repo.workdir()?;
    Some(root.canonicalize().unwrap_or_else(|_| root.to_owned()))
}

/// The view a command means: `wanted`, else the one viewer of `worktree`.
pub(crate) fn pick(
    views: &[View],
    worktree: Option<&Path>,
    wanted: Option<u64>,
) -> Result<View, String> {
    if let Some(id) = wanted {
        return views
            .iter()
            .find(|v| v.id == id)
            .cloned()
            .ok_or_else(|| format!("no view {id} is running; dffy ctl views lists them"));
    }
    let Some(worktree) = worktree else {
        return Err(
            "not in a git working tree: pass --view <id> (dffy ctl views lists them)".into(),
        );
    };
    let here: Vec<&View> = views.iter().filter(|v| v.worktree == worktree).collect();
    match here[..] {
        [one] => Ok(one.clone()),
        [] if views.is_empty() => Err(NO_VIEWER.to_owned()),
        [] => Err(format!(
            "no dffy viewer is running in {}; dffy ctl views lists the {} running elsewhere",
            worktree.display(),
            views.len()
        )),
        _ => {
            let ids: Vec<String> = here.iter().map(|v| v.id.to_string()).collect();
            Err(format!(
                "{} dffy viewers run in {} (views {}); pick one with --view <id>",
                here.len(),
                worktree.display(),
                ids.join(", ")
            ))
        }
    }
}

/// A connection, as the client `name`, to the view `dir`'s working tree
/// has, or `wanted`. Every way of there being no such view is an `Err` in
/// words for the user.
pub(crate) fn reach(dir: &Path, wanted: Option<u64>, name: &str) -> Result<(Client, View), String> {
    let mut client = connect(name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| NO_VIEWER.to_owned())?;
    let views = views(&mut client)?;
    let view = pick(&views, worktree_of(dir).as_deref(), wanted)?;
    Ok((client, view))
}

/// `path`, relative to `cwd` (or absolute), as a path of the working tree
/// at `worktree`, which is canonical: the path needn't exist.
fn in_worktree(cwd: &Path, worktree: &Path, path: &str) -> Result<String, String> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
    let mut full = PathBuf::new();
    for part in cwd.join(path).components() {
        match part {
            Component::ParentDir => {
                full.pop();
            }
            Component::CurDir => {}
            part => full.push(part),
        }
    }
    let inside = full
        .strip_prefix(worktree)
        .map_err(|_| format!("{path} isn't in {}", worktree.display()))?;
    inside
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{path} isn't UTF-8"))
}

/// `path[:line[:old|:new]]`, the line 1-based.
fn place(at: &str) -> (&str, Option<u64>, Option<&'static str>) {
    let (rest, side) = match at.rsplit_once(':') {
        Some((rest, "old")) => (rest, Some("old")),
        Some((rest, "new")) => (rest, Some("new")),
        _ => (at, None),
    };
    match rest.rsplit_once(':') {
        Some((path, line)) if !path.is_empty() => match line.parse() {
            Ok(line) => (path, Some(line), side),
            Err(_) => (rest, None, side),
        },
        _ => (rest, None, side),
    }
}

fn escape(s: &str) -> String {
    crate::render::escape(s.as_bytes())
}

/// `dffy ctl`: the result and, without `--json`, its words.
pub(crate) fn ctl(dir: Option<&Path>, args: &CtlArgs) -> Result<ExitCode, Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| Error::fatal(format!("can't read the current directory: {e}")))?;
    let base = dir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir));
    let (result, words) = match &args.action {
        CtlAction::Schema => {
            io::stdout().lock().write_all(super::SCHEMA.as_bytes())?;
            return Ok(ExitCode::SUCCESS);
        }
        CtlAction::Views => listed()?,
        action => {
            let (mut client, view) =
                reach(&base, args.view, CTL).map_err(|message| Error::Fatal {
                    hint: (message == NO_VIEWER).then(|| {
                        "start dffy in this repository; dffy ctl drives the viewer it opens"
                            .to_owned()
                    }),
                    message,
                })?;
            drive(&mut client, &view, &base, action)?
        }
    };
    let mut out = io::stdout().lock();
    if args.json {
        out.write_all(line(&result).as_bytes())?;
    } else {
        writeln!(out, "{words}")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `dffy ctl views`.
fn listed() -> Result<(Value, String), Error> {
    let views = match connect(CTL)? {
        Some(mut client) => views(&mut client).map_err(Error::fatal)?,
        None => Vec::new(),
    };
    let listed = (views.iter())
        .map(|v| {
            object(vec![
                ("view", number(v.id)),
                ("pid", number(v.pid)),
                ("worktree", text(v.worktree.to_string_lossy())),
                ("diff", text(v.diff.clone())),
            ])
        })
        .collect();
    let words: Vec<String> = (views.iter())
        .map(|v| {
            format!(
                "view {}  pid {}  {}  {}",
                v.id,
                v.pid,
                escape(&v.diff),
                escape(&v.worktree.to_string_lossy())
            )
        })
        .collect();
    let words = if words.is_empty() {
        NO_VIEWER.to_owned()
    } else {
        words.join("\n")
    };
    Ok((object(vec![("views", Value::Array(listed))]), words))
}

/// A `dffy ctl` command for `view`: its result and its words.
#[expect(clippy::too_many_lines, reason = "a dispatch: an arm per command")]
fn drive(
    client: &mut Client,
    view: &View,
    base: &Path,
    action: &CtlAction,
) -> Result<(Value, String), Error> {
    let mut ask = |method: &str, params: Vec<(&str, Value)>| {
        client.ask(view, method, params).map_err(Error::fatal)
    };
    let side = |side: Option<&str>| text(side.unwrap_or("new"));
    let id = view.id;
    Ok(match action {
        CtlAction::Schema | CtlAction::Views => (Value::Null, String::new()),
        CtlAction::State => {
            let state = ask("view/state", Vec::new())?;
            let field = |key: &str| state.get(key).and_then(Value::as_str);
            let top = |side: &str| (state.get("top")?.get(side)?).as_usize();
            let at = match (top("new"), top("old")) {
                (Some(n), _) => format!("line {n}"),
                (None, Some(o)) => format!("old line {o}"),
                (None, None) => "no line on top".to_owned(),
            };
            let shown = field("path").map_or_else(|| "no file".to_owned(), escape);
            let diff = escape(field("diff").unwrap_or_default());
            let words = format!("view {id}  {shown}  {at}  {diff}");
            (state, words)
        }
        CtlAction::Selection => {
            let result = ask("view/selection", Vec::new())?;
            let words = match result.get("selection") {
                Some(s @ Value::Object(_)) => {
                    let field = |key: &str| s.get(key).and_then(Value::as_str).unwrap_or_default();
                    let n = |key: &str| s.get(key).and_then(Value::as_usize).unwrap_or(0);
                    let lines = match (n("start"), n("end")) {
                        (a, b) if a == b => a.to_string(),
                        (a, b) => format!("{a}-{b}"),
                    };
                    let body = field("text");
                    format!(
                        "{}:{lines} {}\n{}",
                        escape(field("path")),
                        field("side"),
                        body.strip_suffix('\n').unwrap_or(body)
                    )
                }
                _ => format!("nothing is selected in view {id}; v selects lines there"),
            };
            (result, words)
        }
        CtlAction::Open { at } => {
            let (path, line, on) = place(at);
            let path = in_worktree(base, &view.worktree, path).map_err(Error::fatal)?;
            let mut params = vec![("path", text(path.clone())), ("side", side(on))];
            params.extend(line.map(|l| ("line", number(l))));
            let result = ask("view/open", params)?;
            let at = line.map_or_else(String::new, |l| format!(":{l}"));
            let notes: Vec<&str> = [
                (on == Some("old")).then_some("old side"),
                (result.get("listed") == Some(&Value::Bool(false))).then_some("not in the diff"),
            ]
            .into_iter()
            .flatten()
            .collect();
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join(", "))
            };
            let words = format!("view {id} shows {}{at}{notes}", escape(&path));
            (result, words)
        }
        CtlAction::Focus { thread } => {
            let result = ask("view/focusThread", vec![("id", text(thread.clone()))])?;
            let path = escape(
                result
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            let thread = escape(thread);
            let words = if result.get("focused") == Some(&Value::Bool(true)) {
                format!("view {id} focused thread {thread} on {path}")
            } else {
                format!("view {id} shows {path} and focuses thread {thread} once it's placed")
            };
            (result, words)
        }
        CtlAction::Comment { at, text: words } => {
            let (path, line, on) = place(at);
            let Some(line) = line else {
                return Err(Error::Fatal {
                    message: format!("{at} has no line"),
                    hint: Some("as in: dffy ctl comment src/lib.rs:42 why this".to_owned()),
                });
            };
            let path = in_worktree(base, &view.worktree, path).map_err(Error::fatal)?;
            let params = vec![
                ("path", text(path.clone())),
                ("line", number(line)),
                ("side", side(on)),
                ("text", text(words.join(" "))),
            ];
            let result = ask("comments/add", params)?;
            let comment = result.get("id").and_then(Value::as_str).unwrap_or_default();
            let unlisted = if result.get("listed") == Some(&Value::Bool(false)) {
                " (not in the diff)"
            } else {
                ""
            };
            let words = format!(
                "comment {comment} on {}:{line}{unlisted}, in view {id}",
                escape(&path)
            );
            (result, words)
        }
        CtlAction::ApplySuggestion { id: comment } => applied(
            id,
            ask(
                "comments/applySuggestion",
                vec![("id", text(comment.clone()))],
            )?,
        ),
    })
}

/// `comments/applySuggestion`'s `result` in view `id`, and what it did in
/// words.
fn applied(id: u64, result: Value) -> (Value, String) {
    let field = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or_default();
    let number = |key: &str| result.get(key).and_then(Value::as_usize).unwrap_or(0);
    let (path, start, end) = (escape(field("path")), number("start"), number("end"));
    let at = if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    };
    let comment = escape(field("comment"));
    let words = match number("lines") {
        0 => format!("view {id} removed {at}, as comment {comment} suggested"),
        _ => {
            format!("view {id} applied comment {comment}'s suggestion to {at}: an unstaged change")
        }
    };
    (result, words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_is_a_path_with_a_line_and_a_side() {
        assert_eq!(place("src/a.rs:42"), ("src/a.rs", Some(42), None));
        assert_eq!(
            place("src/a.rs:42:old"),
            ("src/a.rs", Some(42), Some("old"))
        );
        assert_eq!(place("src/a.rs"), ("src/a.rs", None, None));
        assert_eq!(place("a:b.rs:7"), ("a:b.rs", Some(7), None));
        assert_eq!(place("odd:name"), ("odd:name", None, None));
    }

    #[test]
    fn paths_are_taken_from_the_current_directory_into_the_working_tree() {
        let tree = Path::new("/w");
        assert_eq!(
            in_worktree(Path::new("/w/src"), tree, "a.rs"),
            Ok("src/a.rs".into())
        );
        assert_eq!(
            in_worktree(Path::new("/w/src"), tree, "../b.rs"),
            Ok("b.rs".into())
        );
        assert_eq!(
            in_worktree(Path::new("/x"), tree, "/w/c.rs"),
            Ok("c.rs".into())
        );
        assert!(in_worktree(Path::new("/w"), tree, "../out.rs").is_err());
    }

    /// One viewer in the working tree is it; none or several is an error
    /// that says what to do.
    #[test]
    fn a_command_means_the_one_viewer_of_its_working_tree() {
        let view = |id, worktree: &str| View {
            id,
            pid: id,
            worktree: worktree.into(),
            diff: "uncommitted".into(),
        };
        let views = [view(1, "/a"), view(2, "/b"), view(3, "/b")];
        let picked =
            |worktree: &str, wanted| pick(&views, Some(Path::new(worktree)), wanted).map(|v| v.id);
        assert_eq!(picked("/a", None), Ok(1));
        assert_eq!(picked("/b", Some(3)), Ok(3));
        let several = picked("/b", None).expect_err("two");
        assert!(
            several.contains("views 2, 3") && several.contains("--view"),
            "{several}"
        );
        let none = picked("/c", None).expect_err("none");
        assert!(none.contains("no dffy viewer is running in /c"), "{none}");
        assert_eq!(
            pick(&[], Some(Path::new("/c")), None),
            Err(NO_VIEWER.to_owned())
        );
        assert!(pick(&views, None, Some(9)).is_err());
    }
}
