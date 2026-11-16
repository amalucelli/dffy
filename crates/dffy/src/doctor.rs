//! `dffy doctor`: the environment facts that decide whether dffy works well
//! here, and a quick measurement of the repository. Gathering and judging
//! are separate so the report's shape can be tested with fixed facts; the
//! same checks print as text or as one JSON object.

use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use dffy_core::diff::Options;
use dffy_core::git::{self, ChangeSet, FileChange, Head, Repo, Revisions, Status};
use dffy_core::view::Staging;

use crate::Ctx;
use crate::daemon::protocol::{self, VERSION};
use crate::render::{Canvas, escape, escape_lines, thousands};
use crate::theme::Role;

/// The measurement stops counting changes here, and says so.
const MAX_CHANGES: usize = 50_000;
/// Past this much time spent measuring, the untracked walk isn't timed on
/// its own (on the kernel tree it takes about 70ms), and past the second
/// the largest diff isn't either: the measurement stays around a quarter
/// of a second.
const UNTRACKED_BUDGET: Duration = Duration::from_millis(180);
const DIFF_BUDGET: Duration = Duration::from_millis(240);
/// The largest files tried for the largest diff, binaries being skipped.
const LARGEST_TRIES: usize = 3;

const NOT_A_REPOSITORY: &str = "not in a git repository (-C <path>)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Facts {
    /// This binary's [`crate::BUILD`].
    pub build: &'static str,
    /// `$HOME`, to shorten paths with `~`.
    pub home: Option<PathBuf>,
    pub git: Result<String, String>,
    pub repo: Result<RepoFacts, String>,
    /// `None` outside a repository.
    pub speed: Option<Speed>,
    /// `max_user_watches` and `max_user_instances`; `None` off Linux.
    pub inotify: Option<Result<(u64, u64), String>>,
    /// Watches this user's processes hold, as far as `/proc` shows.
    pub inotify_used: Option<u64>,
    /// Watches watching this repository takes; `None` off Linux or outside
    /// a repository.
    pub watches_needed: Option<u64>,
    /// `None` when no broker runs.
    pub dffyd: Result<Option<protocol::Report>, String>,
    /// Protocol versions of brokers of other dffy versions.
    pub other_brokers: Vec<u32>,
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub term_program: Option<String>,
    pub multiplexer: Option<&'static str>,
    /// tmux's `set-clipboard`, inside tmux and when it can be read.
    pub tmux_clipboard: Option<String>,
    pub width: Option<usize>,
    pub stdout_tty: bool,
    pub color: bool,
    pub config_path: Option<PathBuf>,
    pub config_found: bool,
    pub config_warnings: Vec<String>,
    pub theme: String,
    pub pager: Option<String>,
    /// `None` when the cache is off.
    pub cache: Option<Result<CacheUsage, String>>,
    /// The `render` setting's extensions and renderers, with where each
    /// renderer's executable is, if it's on PATH.
    pub renderers: Vec<(String, String, Option<PathBuf>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoFacts {
    /// The working tree, canonical.
    pub path: PathBuf,
    /// `None` when `HEAD` can't be read.
    pub head: Option<Head>,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    /// Counting stopped at [`MAX_CHANGES`].
    pub capped: bool,
}

/// The repository measured along the viewer's own paths: opening it and
/// resolving the default diff, listing its changes, the untracked walk
/// alone, and the diff of the largest changed text file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Speed {
    pub open: Duration,
    /// `None` without changes.
    pub first: Option<Duration>,
    pub all: Duration,
    pub changes: usize,
    pub capped: bool,
    /// `None` when skipped for time.
    pub untracked: Option<Duration>,
    /// The file and its diff's time; `None` without a text file to diff.
    pub largest: Option<(String, Duration)>,
    /// The largest diff was skipped for time.
    pub largest_skipped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheUsage {
    pub dir: PathBuf,
    pub bytes: u64,
    pub entries: usize,
    pub budget: u64,
}

impl Facts {
    pub(crate) fn gather(ctx: &Ctx, dir: Option<&Path>) -> Self {
        let env = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
        let git = match Command::new("git").arg("--version").output() {
            Ok(out) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
            }
            Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            Err(e) => Err(e.to_string()),
        };
        let options = Options {
            context: ctx.context_lines,
            whitespace: ctx.config.whitespace,
        };
        let start = dir.map_or_else(|| PathBuf::from("."), Path::to_owned);
        let (repo, speed, root) = match measure(&start, &options) {
            Ok((repo, speed, root)) => (Ok(repo), Some(speed), root),
            Err(e) => (Err(e), None, None),
        };
        #[cfg(target_os = "linux")]
        let watches_needed = root
            .as_ref()
            .map(|root| u64::try_from(dffy_core::watch::watches_needed(root)).unwrap_or(u64::MAX));
        #[cfg(not(target_os = "linux"))]
        let watches_needed = {
            drop(root);
            None
        };
        let inotify = cfg!(target_os = "linux").then(|| {
            let read = |name: &str| -> Result<u64, String> {
                let path = format!("/proc/sys/fs/inotify/{name}");
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("{path}: {e}"))?
                    .trim()
                    .parse()
                    .map_err(|e| format!("{path}: {e}"))
            };
            Ok((read("max_user_watches")?, read("max_user_instances")?))
        });
        let dffyd = crate::daemon::report().map_err(|e| e.to_string());
        let multiplexer = if env("TMUX").is_some() {
            Some("tmux")
        } else if env("ZELLIJ").is_some() {
            Some("zellij")
        } else if env("STY").is_some() {
            Some("screen")
        } else if env("TERM_PROGRAM").is_some_and(|p| p == "herdr") {
            // herdr sets no variable of its own; it names itself in TERM_PROGRAM.
            Some("herdr")
        } else {
            None
        };
        Self {
            build: crate::BUILD,
            home: std::env::var_os("HOME").map(PathBuf::from),
            git,
            repo,
            speed,
            inotify_used: inotify.is_some().then(inotify_watches_in_use).flatten(),
            watches_needed,
            inotify,
            dffyd,
            other_brokers: crate::daemon::other_brokers(),
            term: env("TERM"),
            colorterm: env("COLORTERM"),
            term_program: env("TERM_PROGRAM"),
            multiplexer,
            tmux_clipboard: crate::term::tmux_clipboard(),
            width: ctx.term.width,
            stdout_tty: ctx.term.stdout_tty,
            color: ctx.color_out,
            config_path: ctx
                .config_path
                .clone()
                .or_else(|| crate::config::config_dir().map(|d| d.join("config"))),
            config_found: ctx.config_path.is_some(),
            config_warnings: ctx
                .config_warnings
                .iter()
                .map(|w| match w.line {
                    0 => w.message.clone(),
                    n => format!("line {n}: {}", w.message),
                })
                .collect(),
            theme: ctx.theme.name.clone(),
            pager: crate::output::pager_command(&ctx.config.pager),
            cache: ctx.cache.as_ref().map(|cache| {
                let (bytes, entries) = cache
                    .usage()
                    .map_err(|e| format!("{}: {e}", cache.dir().display()))?;
                Ok(CacheUsage {
                    dir: cache.dir().to_owned(),
                    bytes,
                    entries,
                    budget: ctx.config.cache_size,
                })
            }),
            renderers: renderers(&ctx.config.render),
        }
    }

    /// Writes the report as text. Returns whether any check failed.
    pub(crate) fn render(&self, c: &mut Canvas) -> bool {
        let checks = self.checks();
        for check in &checks {
            let (symbol, role) = match check.mark {
                Mark::Ok => ("✓", Role::Ok),
                Mark::Warn => ("!", Role::Warn),
                Mark::Fail => ("✗", Role::Error),
                Mark::Info => ("·", Role::Dim),
            };
            c.push(role, symbol);
            c.push_fmt(Role::Plain, format_args!(" {:<12}", check.label));
            // Details quote paths, config values and environment variables.
            c.line(Role::Plain, &escape_lines(&check.detail));
            for note in &check.notes {
                c.push(Role::Plain, "  ");
                c.line(Role::Warn, &escape_lines(note));
            }
            for hint in &check.hints {
                c.push(Role::Plain, "  ");
                c.push(Role::Hint, "hint:");
                c.line(Role::Plain, &format!(" {}", escape_lines(hint)));
            }
        }
        failed(&checks)
    }

    /// The report as one JSON object: each check as text renders it, then
    /// the facts and measurements behind them, durations in milliseconds.
    /// Every key is always there, `null` when it doesn't apply. Returns the
    /// object and whether any check failed.
    pub(crate) fn json(&self) -> (String, bool) {
        let checks = self.checks();
        let failed = failed(&checks);
        let checks = checks
            .iter()
            .map(|check| {
                Json::Object(vec![
                    ("name", Json::str(check.label)),
                    ("status", Json::str(check.mark.name())),
                    ("detail", Json::str(&check.detail)),
                    ("notes", Json::strings(&check.notes)),
                    ("hints", Json::strings(&check.hints)),
                ])
            })
            .collect();
        let opt_str = |s: &Option<String>| s.as_ref().map_or(Json::Null, Json::str);
        let object = Json::Object(vec![
            ("build", Json::str(self.build)),
            ("failed", Json::Bool(failed)),
            ("checks", Json::Array(checks)),
            (
                "git",
                Json::Object(vec![
                    (
                        "version",
                        self.git.as_ref().ok().map_or(Json::Null, Json::str),
                    ),
                    ("error", Json::error(self.git.as_ref())),
                ]),
            ),
            ("repository", self.repository_json()),
            ("watch", self.watch_json()),
            ("speed", self.speed_json()),
            (
                "terminal",
                Json::Object(vec![
                    ("term", opt_str(&self.term)),
                    ("colorterm", opt_str(&self.colorterm)),
                    ("term_program", opt_str(&self.term_program)),
                    ("width", Json::opt(self.width)),
                    ("stdout_tty", Json::Bool(self.stdout_tty)),
                    (
                        "multiplexer",
                        self.multiplexer.map_or(Json::Null, Json::str),
                    ),
                    ("tmux_clipboard", opt_str(&self.tmux_clipboard)),
                    ("color", Json::Bool(self.color)),
                ]),
            ),
            (
                "config",
                Json::Object(vec![
                    (
                        "path",
                        self.config_path.as_ref().map_or(Json::Null, Json::path),
                    ),
                    ("found", Json::Bool(self.config_found)),
                    ("warnings", Json::strings(&self.config_warnings)),
                    ("theme", Json::str(&self.theme)),
                    ("pager", opt_str(&self.pager)),
                ]),
            ),
            ("cache", self.cache_json()),
            ("dffyd", self.broker_json()),
            (
                "renderers",
                Json::Array(
                    self.renderers
                        .iter()
                        .map(|(ext, name, found)| {
                            Json::Object(vec![
                                ("extension", Json::str(ext)),
                                ("name", Json::str(name)),
                                ("path", found.as_ref().map_or(Json::Null, Json::path)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]);
        let mut out = String::new();
        object.write(&mut out);
        out.push('\n');
        (out, failed)
    }

    fn repository_json(&self) -> Json {
        let repo = self.repo.as_ref().ok();
        let (head, branch, commit) = match repo.and_then(|r| r.head.as_ref()) {
            Some(Head::Branch(name)) => (Json::str("branch"), Json::str(name), Json::Null),
            Some(Head::Unborn(name)) => (Json::str("unborn"), Json::str(name), Json::Null),
            Some(Head::Detached(id)) => {
                (Json::str("detached"), Json::Null, Json::str(id.to_string()))
            }
            None => (Json::Null, Json::Null, Json::Null),
        };
        let count = |f: fn(&RepoFacts) -> usize| repo.map_or(Json::Null, |r| Json::int(f(r)));
        Json::Object(vec![
            ("path", repo.map_or(Json::Null, |r| Json::path(&r.path))),
            ("head", head),
            ("branch", branch),
            ("commit", commit),
            ("staged", count(|r| r.staged)),
            ("unstaged", count(|r| r.unstaged)),
            ("untracked", count(|r| r.untracked)),
            ("capped", repo.map_or(Json::Null, |r| Json::Bool(r.capped))),
            ("error", Json::error(self.repo.as_ref())),
        ])
    }

    fn watch_json(&self) -> Json {
        let limits = self.inotify.as_ref().and_then(|r| r.as_ref().ok());
        let backend = if self.inotify.is_some() {
            "inotify"
        } else {
            "fsevents"
        };
        Json::Object(vec![
            ("backend", Json::str(backend)),
            ("needed", Json::opt(self.watches_needed)),
            ("in_use", Json::opt(self.inotify_used)),
            ("limit", Json::opt(limits.map(|l| l.0))),
            ("instances", Json::opt(limits.map(|l| l.1))),
            (
                "error",
                self.inotify
                    .as_ref()
                    .map_or(Json::Null, |r| Json::error(r.as_ref())),
            ),
        ])
    }

    fn speed_json(&self) -> Json {
        let s = self.speed.as_ref();
        let ms = |d: Option<Duration>| d.map_or(Json::Null, Json::Ms);
        let largest = s.and_then(|s| s.largest.as_ref());
        Json::Object(vec![
            ("repo_ms", ms(s.map(|s| s.open))),
            ("first_change_ms", ms(s.and_then(|s| s.first))),
            ("all_changes_ms", ms(s.map(|s| s.all))),
            ("changes", s.map_or(Json::Null, |s| Json::int(s.changes))),
            ("capped", s.map_or(Json::Null, |s| Json::Bool(s.capped))),
            ("untracked_ms", ms(s.and_then(|s| s.untracked))),
            ("largest_diff_ms", ms(largest.map(|l| l.1))),
            (
                "largest_diff_path",
                largest.map_or(Json::Null, |l| Json::str(&l.0)),
            ),
        ])
    }

    fn cache_json(&self) -> Json {
        let usage = self.cache.as_ref().and_then(|c| c.as_ref().ok());
        match &self.cache {
            None => Json::Null,
            Some(cache) => Json::Object(vec![
                ("dir", usage.map_or(Json::Null, |u| Json::path(&u.dir))),
                ("bytes", usage.map_or(Json::Null, |u| Json::int(u.bytes))),
                (
                    "entries",
                    usage.map_or(Json::Null, |u| Json::int(u.entries)),
                ),
                ("budget", usage.map_or(Json::Null, |u| Json::int(u.budget))),
                ("error", Json::error(cache.as_ref())),
            ]),
        }
    }

    fn broker_json(&self) -> Json {
        let report = self.dffyd.as_ref().ok().and_then(Option::as_ref);
        let root = |root: &protocol::RootReport| {
            Json::Object(vec![
                ("path", Json::path(&root.path)),
                ("subscribers", Json::int(root.subscribers)),
                ("watches", Json::int(root.watches)),
                ("backend", Json::str(&root.backend)),
                (
                    "polling",
                    root.polling.as_ref().map_or(Json::Null, Json::str),
                ),
            ])
        };
        Json::Object(vec![
            ("running", Json::Bool(report.is_some())),
            ("pid", report.map_or(Json::Null, |r| Json::int(r.pid))),
            (
                "uptime_secs",
                report.map_or(Json::Null, |r| Json::int(r.uptime_secs)),
            ),
            (
                "build",
                report
                    .and_then(|r| r.build.as_ref())
                    .map_or(Json::Null, Json::str),
            ),
            (
                "current",
                report.map_or(Json::Null, |r| Json::Bool(self.current(r))),
            ),
            ("protocol", Json::int(VERSION)),
            (
                "roots",
                report.map_or(Json::Null, |r| {
                    Json::Array(r.roots.iter().map(root).collect())
                }),
            ),
            (
                "other_protocols",
                Json::Array(self.other_brokers.iter().map(|&v| Json::int(v)).collect()),
            ),
            ("error", Json::error(self.dffyd.as_ref())),
        ])
    }

    fn checks(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        self.repository(&mut checks);
        self.watch(&mut checks);
        self.speed(&mut checks);
        self.terminal(&mut checks);
        self.settings(&mut checks);
        self.broker(&mut checks);
        checks
    }

    /// `path` with the home directory as `~`.
    fn short(&self, path: &Path) -> String {
        match self
            .home
            .as_ref()
            .and_then(|home| path.strip_prefix(home).ok())
        {
            Some(rel) if rel.as_os_str().is_empty() => "~".to_owned(),
            Some(rel) => format!("~/{}", rel.display()),
            None => path.display().to_string(),
        }
    }

    /// Whether `report` comes from a broker of this very build.
    fn current(&self, report: &protocol::Report) -> bool {
        report.build.as_deref() == Some(self.build)
    }

    fn repository(&self, checks: &mut Vec<Check>) {
        match &self.git {
            Ok(version) => checks.push(Check::new(Mark::Ok, "git", version)),
            Err(e) => checks.push(
                Check::new(Mark::Fail, "git", format!("not found: {e}"))
                    .hint("install git; dffy reads repositories itself but checks against git"),
            ),
        }
        let repo = match &self.repo {
            Ok(repo) => repo,
            Err(e) => {
                checks.push(Check::new(Mark::Info, "repository", e));
                return;
            }
        };
        let branch = match &repo.head {
            Some(Head::Branch(name)) => Some(name.clone()),
            Some(Head::Unborn(name)) => Some(format!("{name} (no commits yet)")),
            Some(Head::Detached(id)) => Some(format!("detached at {}", id.to_hex_with_len(7))),
            None => None,
        };
        let counts: Vec<String> = [
            (repo.staged, "staged"),
            (repo.unstaged, "unstaged"),
            (repo.untracked, "untracked"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, kind)| format!("{} {kind}", thousands(*n)))
        .collect();
        let mut counts = if counts.is_empty() {
            "nothing uncommitted".to_owned()
        } else {
            counts.join(", ")
        };
        if repo.capped {
            let _ = write!(
                counts,
                " (counting stopped at {} changes)",
                thousands(MAX_CHANGES)
            );
        }
        let detail: Vec<String> = [Some(self.short(&repo.path)), branch, Some(counts)]
            .into_iter()
            .flatten()
            .collect();
        checks.push(Check::new(Mark::Ok, "repository", detail.join(" · ")));
    }

    fn watch(&self, checks: &mut Vec<Check>) {
        let label = "watch";
        let (limit, used) = match &self.inotify {
            None => {
                checks.push(Check::new(
                    Mark::Info,
                    label,
                    "FSEvents needs no per-directory watches",
                ));
                return;
            }
            Some(Err(e)) => {
                checks.push(Check::new(Mark::Warn, label, format!("can't read: {e}")));
                return;
            }
            Some(Ok((limit, _))) => (*limit, self.inotify_used),
        };
        let mut parts = Vec::new();
        if let Some(needed) = self.watches_needed {
            parts.push(format!("{} needed here", thousands_u64(needed)));
        }
        parts.push(match used {
            Some(used) => format!("{} of {} in use", thousands_u64(used), thousands_u64(limit)),
            None => format!("{} allowed", thousands_u64(limit)),
        });
        // A broker already watching this repository holds what it needs.
        let held = self.held_by_broker();
        if held > 0 {
            parts.push("dffyd holds them already".to_owned());
        }
        let detail = parts.join(" · ");
        let free = limit.saturating_sub(used.unwrap_or(0)) + held;
        match self.watches_needed {
            Some(needed) if needed > free => checks.push(
                Check::new(Mark::Warn, label, detail)
                    .note(format!(
                        "only {} free: watch mode would poll this repository once a second",
                        thousands_u64(free)
                    ))
                    .hint("raise the limit: sudo sysctl fs.inotify.max_user_watches=524288"),
            ),
            _ => checks.push(Check::new(Mark::Ok, label, detail)),
        }
    }

    /// Watches the running broker holds for this repository.
    fn held_by_broker(&self) -> u64 {
        let (Ok(Some(report)), Ok(repo)) = (&self.dffyd, &self.repo) else {
            return 0;
        };
        report
            .roots
            .iter()
            .filter(|root| root.path == repo.path)
            .map(|root| u64::from(root.watches))
            .sum()
    }

    fn speed(&self, checks: &mut Vec<Check>) {
        let Some(s) = &self.speed else {
            return;
        };
        let mut parts = vec![format!("repo {}", millis(s.open))];
        if let Some(first) = s.first {
            parts.push(format!("first change {}", millis(first)));
        }
        let changes = if s.changes == 1 { "change" } else { "changes" };
        parts.push(format!(
            "all {}{} {changes} {}",
            thousands(s.changes),
            if s.capped { "+" } else { "" },
            millis(s.all)
        ));
        parts.push(match s.untracked {
            Some(walk) => format!("untracked {}", millis(walk)),
            None => "untracked not timed".to_owned(),
        });
        if let Some((path, took)) = &s.largest {
            parts.push(format!("largest diff {} ({path})", millis(*took)));
        } else if s.largest_skipped {
            parts.push("largest diff not timed".to_owned());
        }
        let mut check = Check::new(Mark::Info, "speed", parts.join(" · "));
        if s.capped {
            check = check.note(format!(
                "stopped counting at {} changes; the viewer lists them all",
                thousands(MAX_CHANGES)
            ));
        }
        if s.untracked.is_none() || s.largest_skipped {
            check = check.note("the rest wasn't timed, to keep doctor quick on a large repository");
        }
        checks.push(check);
    }

    fn broker(&self, checks: &mut Vec<Check>) {
        let label = "dffyd";
        match &self.dffyd {
            Ok(None) => checks.push(Check::new(
                Mark::Info,
                label,
                "not running; the first dffy --watch starts it",
            )),
            Ok(Some(report)) => {
                let subscribers: u32 = report.roots.iter().map(|root| root.subscribers).sum();
                let running = format!(
                    "running (pid {}), {} root(s), {subscribers} subscriber(s)",
                    report.pid,
                    report.roots.len()
                );
                let mut check = if self.current(report) {
                    Check::new(Mark::Ok, label, running)
                } else {
                    let build = match &report.build {
                        Some(build) => format!("build {build}"),
                        None => "a build from before brokers reported theirs".to_owned(),
                    };
                    Check::new(
                        Mark::Warn,
                        label,
                        format!("{running}; it's {build}, this dffy is {}", self.build),
                    )
                    .hint("dffy daemon stop; the next dffy --watch starts this build's broker")
                };
                for root in report.roots.iter().filter(|root| root.polling.is_some()) {
                    check = check.note(format!(
                        "{} is polled: {}",
                        root.path.display(),
                        root.polling.as_deref().unwrap_or_default()
                    ));
                }
                checks.push(check);
            }
            Err(e) => checks.push(
                Check::new(Mark::Warn, label, format!("not answering: {e}")).hint(
                    "remove its stale socket under $XDG_RUNTIME_DIR/dffy; \
                     views watch by themselves meanwhile",
                ),
            ),
        }
        for version in &self.other_brokers {
            checks.push(
                Check::new(
                    Mark::Warn,
                    label,
                    format!("a broker of protocol v{version} runs too, from another dffy version"),
                )
                .hint(
                    "it serves that version's views and exits a minute after the last one; \
                     dffy daemon stop from that version stops it now",
                ),
            );
        }
    }

    fn terminal(&self, checks: &mut Vec<Check>) {
        let mut terminal = vec![
            format!("TERM={}", self.term.as_deref().unwrap_or("unset")),
            format!("COLORTERM={}", self.colorterm.as_deref().unwrap_or("unset")),
        ];
        if let Some(program) = &self.term_program {
            terminal.push(format!("TERM_PROGRAM={program}"));
        }
        terminal.push(match (self.stdout_tty, self.width) {
            (true, Some(w)) => format!("{w} columns"),
            (true, None) => "width unknown".to_owned(),
            (false, _) => "stdout is not a terminal".to_owned(),
        });
        let mark = if self.term.as_deref() == Some("dumb") {
            Mark::Warn
        } else {
            Mark::Ok
        };
        checks.push(Check::new(mark, "terminal", terminal.join(", ")));
        checks.push(match self.multiplexer {
            Some(name) => Check::new(
                Mark::Ok,
                "multiplexer",
                format!(
                    "{name}; dffy's one query (the background color) falls back to dark if it isn't relayed"
                ),
            ),
            None => Check::new(Mark::Info, "multiplexer", "none detected"),
        });
        if self.multiplexer == Some("tmux") {
            let label = "copy";
            checks.push(match self.tmux_clipboard.as_deref() {
                Some("on") => Check::new(
                    Mark::Ok,
                    label,
                    "tmux passes copies (y, Y) to the terminal: set-clipboard is on",
                ),
                value => {
                    let detail = match value {
                        Some(value) => {
                            format!("tmux drops copies (y, Y): set-clipboard is {value}")
                        }
                        None => {
                            "can't read tmux's set-clipboard; copies (y, Y) need it on".to_owned()
                        }
                    };
                    Check::new(Mark::Warn, label, detail).hint(
                        "run tmux set -s set-clipboard on, and add set -s set-clipboard on \
                         to ~/.tmux.conf to keep it",
                    )
                }
            });
        }
        let truecolor = matches!(self.colorterm.as_deref(), Some("truecolor" | "24bit"));
        checks.push(Check::new(
            Mark::Ok,
            "color",
            format!(
                "{}; the default theme uses the 16 ANSI colors{}",
                if self.color { "on" } else { "off" },
                if truecolor {
                    ", truecolor themes work too"
                } else {
                    ""
                }
            ),
        ));
    }

    fn settings(&self, checks: &mut Vec<Check>) {
        let path = self.config_path.as_ref().map_or_else(
            || "no config directory".to_owned(),
            |p| p.display().to_string(),
        );
        if !self.config_found {
            checks.push(Check::new(
                Mark::Info,
                "config",
                format!("{path} (not present; using defaults)"),
            ));
        } else if self.config_warnings.is_empty() {
            checks.push(Check::new(Mark::Ok, "config", path));
        } else {
            let count = self.config_warnings.len();
            let mut check = Check::new(Mark::Warn, "config", format!("{path}: {count} problem(s)"));
            check.notes.clone_from(&self.config_warnings);
            checks.push(check);
        }
        checks.push(Check::new(Mark::Ok, "theme", &self.theme));
        checks.push(Check::new(
            Mark::Ok,
            "pager",
            self.pager
                .as_deref()
                .unwrap_or("none; output goes straight to the terminal"),
        ));
        checks.push(match &self.cache {
            None => Check::new(Mark::Info, "cache", "off"),
            Some(Ok(usage)) => Check::new(
                Mark::Ok,
                "cache",
                format!(
                    "{}: {} in {} entries, trimmed at {}",
                    usage.dir.display(),
                    megabytes(usage.bytes),
                    usage.entries,
                    megabytes(usage.budget)
                ),
            ),
            Some(Err(e)) => Check::new(Mark::Warn, "cache", format!("can't read: {e}"))
                .hint("fix its permissions or set cache = false; dffy works without it"),
        });
        for (ext, name, found) in &self.renderers {
            checks.push(match found {
                Some(path) => Check::new(
                    Mark::Ok,
                    "renderer",
                    format!(".{ext} → {name}: {}", self.short(path)),
                ),
                None => Check::new(
                    Mark::Warn,
                    "renderer",
                    format!(
                        ".{ext} → {name}: {}{name} isn't on PATH",
                        crate::plugin::PREFIX
                    ),
                )
                .hint("install it, or see docs/plugins.md; files of this type show as lines"),
            });
        }
    }
}

/// Each configured renderer with its executable, if it's on PATH.
fn renderers(render: &crate::plugin::Renderers) -> Vec<(String, String, Option<PathBuf>)> {
    render
        .pairs()
        .iter()
        .map(|(ext, name)| {
            let found = crate::plugin::find(name).map(|p| p.path);
            (ext.clone(), name.clone(), found)
        })
        .collect()
}

fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.mark == Mark::Fail)
}

/// Opens the repository at `start` and measures it. Returns its facts, the
/// measurement, and what a watch on it covers.
fn measure(
    start: &Path,
    options: &Options,
) -> Result<(RepoFacts, Speed, Option<dffy_core::watch::Root>), String> {
    let started = Instant::now();
    let repo = Repo::discover(start).map_err(|e| match e {
        git::Error::NotARepository { .. } => NOT_A_REPOSITORY.to_owned(),
        e => e.to_string(),
    })?;
    let revisions = Revisions::default();
    let spec = repo
        .resolve_revisions(&revisions)
        .map_err(|e| e.to_string())?;
    let open = started.elapsed();
    let root = repo.watch_root().ok();
    let workdir = repo.workdir().ok_or("the repository is bare")?.to_owned();
    let mut facts = RepoFacts {
        path: root
            .as_ref()
            .map_or_else(|| workdir.clone(), |r| r.worktree.clone()),
        head: repo.head().ok(),
        staged: 0,
        unstaged: 0,
        untracked: 0,
        capped: false,
    };
    let mut changes = Vec::new();
    let mut first = None;
    let listing = Instant::now();
    let mut set = ChangeSet::new(repo.clone(), spec, revisions, &[], false);
    set.enumerate(|change| {
        first.get_or_insert_with(|| listing.elapsed());
        match (&change.status, change.staging) {
            (Status::Untracked, _) => facts.untracked += 1,
            (_, Some(Staging::Staged)) => facts.staged += 1,
            (_, Some(Staging::Partly)) => {
                facts.staged += 1;
                facts.unstaged += 1;
            }
            _ => facts.unstaged += 1,
        }
        changes.push(change);
        if changes.len() >= MAX_CHANGES {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .map_err(|e| e.to_string())?;
    let all = listing.elapsed();
    let count = changes.len();
    facts.capped = count >= MAX_CHANGES;
    let untracked = (started.elapsed() < UNTRACKED_BUDGET)
        .then(|| {
            let walk = Instant::now();
            repo.untracked().ok().map(|_| walk.elapsed())
        })
        .flatten();
    let largest_skipped = started.elapsed() >= DIFF_BUDGET;
    let largest = if largest_skipped {
        None
    } else {
        largest_diff(&repo, &workdir, &changes, options)
    };
    let speed = Speed {
        open,
        first,
        all,
        changes: count,
        capped: facts.capped,
        untracked,
        largest,
        largest_skipped,
    };
    Ok((facts, speed, root))
}

/// Diffs the largest changed files, by their size in the working tree,
/// until one is text. Returns its path and the diff's time.
fn largest_diff(
    repo: &Repo,
    workdir: &Path,
    changes: &[FileChange],
    options: &Options,
) -> Option<(String, Duration)> {
    let size = |change: &FileChange| {
        change.new.and_then(|_| {
            let path = workdir.join(std::ffi::OsStr::from_bytes(&change.path));
            std::fs::symlink_metadata(path)
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map(|m| m.len())
        })
    };
    let mut sized: Vec<(u64, usize)> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, change)| Some((size(change)?, i)))
        .collect();
    sized.sort_unstable_by(|a, b| b.cmp(a));
    for &(_, i) in sized.iter().take(LARGEST_TRIES) {
        let path = escape(&changes[i].path);
        let started = Instant::now();
        let Ok(diff) = repo.file_diff(changes[i].clone(), options) else {
            continue;
        };
        if !diff.binary && !diff.too_large {
            return Some((path, started.elapsed()));
        }
    }
    None
}

/// `3ms`, or `<1ms`.
fn millis(d: Duration) -> String {
    match d.as_millis() {
        0 => "<1ms".to_owned(),
        ms => format!("{ms}ms"),
    }
}

fn thousands_u64(n: u64) -> String {
    thousands(usize::try_from(n).unwrap_or(usize::MAX))
}

fn megabytes(bytes: u64) -> String {
    let tenths = (bytes * 10) >> 20;
    format!("{}.{}MB", tenths / 10, tenths % 10)
}

/// Counts the watches in every inotify instance this user's processes hold.
/// Processes of others aren't readable, and their watches don't count
/// against this user's limit anyway.
fn inotify_watches_in_use() -> Option<u64> {
    let uid = rustix::process::getuid().as_raw();
    let mut total = 0;
    for process in std::fs::read_dir("/proc").ok()?.flatten() {
        if !process
            .file_name()
            .as_bytes()
            .iter()
            .all(u8::is_ascii_digit)
            || process.metadata().map(|m| m.uid()).ok() != Some(uid)
        {
            continue;
        }
        let path = process.path();
        for fd in std::fs::read_dir(path.join("fd"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if std::fs::read_link(fd.path()).is_ok_and(|t| t.as_os_str() == "anon_inode:inotify")
                && let Ok(info) = std::fs::read_to_string(path.join("fdinfo").join(fd.file_name()))
            {
                total += info
                    .lines()
                    .filter(|l| l.starts_with("inotify wd:"))
                    .count() as u64;
            }
        }
    }
    Some(total)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Ok,
    Warn,
    Fail,
    Info,
}

impl Mark {
    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Info => "info",
        }
    }
}

/// One line of the report, with the lines under it.
struct Check {
    mark: Mark,
    label: &'static str,
    detail: String,
    /// Indented lines in the warning color.
    notes: Vec<String>,
    hints: Vec<String>,
}

impl Check {
    fn new(mark: Mark, label: &'static str, detail: impl Into<String>) -> Self {
        Self {
            mark,
            label,
            detail: detail.into(),
            notes: Vec::new(),
            hints: Vec::new(),
        }
    }

    fn note(mut self, text: impl Into<String>) -> Self {
        self.notes.push(text.into());
        self
    }

    fn hint(mut self, text: impl Into<String>) -> Self {
        self.hints.push(text.into());
        self
    }
}

/// Just enough JSON to write the report.
enum Json {
    Null,
    Bool(bool),
    Int(u64),
    /// Milliseconds, to a tenth.
    Ms(Duration),
    Str(String),
    Array(Vec<Self>),
    Object(Vec<(&'static str, Self)>),
}

impl Json {
    fn str(s: impl AsRef<str>) -> Self {
        Self::Str(s.as_ref().to_owned())
    }

    fn int(n: impl TryInto<u64>) -> Self {
        Self::Int(n.try_into().unwrap_or(u64::MAX))
    }

    fn path(path: impl AsRef<Path>) -> Self {
        Self::Str(path.as_ref().display().to_string())
    }

    fn strings(list: &[String]) -> Self {
        Self::Array(list.iter().map(Self::str).collect())
    }

    /// The error of `result`, `null` when it's fine.
    fn error<T>(result: Result<T, &String>) -> Self {
        result.err().map_or(Self::Null, Self::str)
    }

    fn opt<T: TryInto<u64>>(n: Option<T>) -> Self {
        n.and_then(|n| n.try_into().ok())
            .map_or(Self::Null, Self::Int)
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Ms(d) => {
                let tenths = d.as_micros() / 100;
                let _ = write!(out, "{}.{}", tenths / 10, tenths % 10);
            }
            Self::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        c if u32::from(c) < 0x20 => {
                            let _ = write!(out, "\\u{:04x}", u32::from(c));
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    Self::Str((*key).to_owned()).write(out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILD: &str = "0.1.0+0123abcd";

    fn facts() -> Facts {
        Facts {
            build: BUILD,
            home: Some(PathBuf::from("/home/u")),
            git: Ok("git version 2.43.0".to_owned()),
            repo: Ok(RepoFacts {
                path: PathBuf::from("/home/u/src/linux"),
                head: Some(Head::Branch("main".to_owned())),
                staged: 265,
                unstaged: 3,
                untracked: 4,
                capped: false,
            }),
            speed: Some(Speed {
                open: Duration::from_micros(3_200),
                first: Some(Duration::from_millis(9)),
                all: Duration::from_millis(41),
                changes: 270,
                capped: false,
                untracked: Some(Duration::from_millis(18)),
                largest: Some(("Cargo.lock".to_owned(), Duration::from_micros(2_250))),
                largest_skipped: false,
            }),
            inotify: Some(Ok((129_194, 128))),
            inotify_used: Some(5_259),
            watches_needed: Some(67),
            dffyd: Ok(Some(protocol::Report {
                pid: 4242,
                uptime_secs: 75,
                roots: vec![protocol::RootReport {
                    path: PathBuf::from("/home/u/src/linux"),
                    subscribers: 3,
                    watches: 67,
                    backend: "inotify".to_owned(),
                    polling: None,
                }],
                build: Some(BUILD.to_owned()),
            })),
            other_brokers: Vec::new(),
            term: Some("xterm-ghostty".to_owned()),
            colorterm: Some("truecolor".to_owned()),
            term_program: Some("ghostty".to_owned()),
            multiplexer: Some("tmux"),
            tmux_clipboard: Some("on".to_owned()),
            width: Some(160),
            stdout_tty: true,
            color: true,
            config_path: Some(PathBuf::from("/home/u/.config/dffy/config")),
            config_found: true,
            config_warnings: Vec::new(),
            theme: "default".to_owned(),
            pager: Some("less".to_owned()),
            cache: Some(Ok(CacheUsage {
                dir: PathBuf::from("/home/u/.cache/dffy"),
                bytes: 12 << 20,
                entries: 40,
                budget: 256 << 20,
            })),
            renderers: vec![
                (
                    "md".to_owned(),
                    "markdown".to_owned(),
                    Some(PathBuf::from("/home/u/.local/bin/dffy-render-markdown")),
                ),
                ("csv".to_owned(), "csv".to_owned(), None),
            ],
        }
    }

    fn text(facts: &Facts) -> (String, bool) {
        let mut c = Canvas::default();
        let failed = facts.render(&mut c);
        (c.markup(), failed)
    }

    /// The lines of `text` from the one labelled `label`, with what's
    /// indented under it.
    fn check<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
        let mut lines = text
            .lines()
            .skip_while(|l| !l.contains(&format!(" {label:<12}")));
        let first = lines.next().into_iter();
        first
            .chain(lines.take_while(|l| l.starts_with("  ")))
            .collect()
    }

    #[test]
    fn healthy_environment() {
        let (text, failed) = text(&facts());
        assert!(!failed);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn problems_get_marks_and_hints() {
        let facts = Facts {
            git: Err("No such file or directory (os error 2)".to_owned()),
            inotify: Some(Ok((8192, 128))),
            inotify_used: Some(8_150),
            watches_needed: Some(6_345),
            dffyd: Err("Connection refused (os error 111)".to_owned()),
            term: Some("dumb".to_owned()),
            colorterm: None,
            term_program: None,
            multiplexer: None,
            stdout_tty: false,
            color: false,
            config_warnings: vec!["line 3: unknown key 'colour'".to_owned()],
            pager: None,
            cache: Some(Err(
                "/home/u/.cache/dffy: Permission denied (os error 13)".to_owned()
            )),
            ..facts()
        };
        let (text, failed) = text(&facts);
        assert!(failed);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn outside_a_repository() {
        let facts = Facts {
            repo: Err(NOT_A_REPOSITORY.to_owned()),
            speed: None,
            watches_needed: None,
            ..facts()
        };
        let (text, failed) = text(&facts);
        assert!(!failed);
        insta::assert_snapshot!(check(&text, "repository").join("\n"), @"<dim>·</> repository  not in a git repository (-C <path>)");
        insta::assert_snapshot!(check(&text, "watch").join("\n"), @"<ok>✓</> watch       5,259 of 129,194 in use");
        assert!(check(&text, "speed").is_empty(), "nothing to measure");
    }

    /// A new repository, a detached `HEAD`, a clean tree and a huge one.
    #[test]
    fn the_repository_line_names_the_branch_and_counts() {
        let line = |head, staged, capped| {
            let mut facts = facts();
            if let Ok(repo) = &mut facts.repo {
                (
                    repo.head,
                    repo.staged,
                    repo.unstaged,
                    repo.untracked,
                    repo.capped,
                ) = (head, staged, 0, 0, capped);
            }
            check(&text(&facts).0, "repository").join("\n")
        };
        let id = git::ObjectId::from_hex(b"80f116f2b0d8f3f6c0a1e2d3c4b5a69788990011").ok();
        insta::assert_snapshot!(
            [
                line(Some(Head::Unborn("main".to_owned())), 265, false),
                line(id.map(Head::Detached), 0, false),
                line(Some(Head::Branch("main".to_owned())), 50_000, true),
            ]
            .join("\n"),
            @r"
        <ok>✓</> repository  ~/src/linux · main (no commits yet) · 265 staged
        <ok>✓</> repository  ~/src/linux · detached at 80f116f · nothing uncommitted
        <ok>✓</> repository  ~/src/linux · main · 50,000 staged (counting stopped at 50,000 changes)
        "
        );
    }

    /// A broker from another build, or one too old to say, gets a warning
    /// and how to replace it.
    #[test]
    fn a_broker_of_another_build_is_flagged() {
        for build in [Some("0.1.0+feedface".to_owned()), None] {
            let mut facts = facts();
            if let Ok(Some(report)) = &mut facts.dffyd {
                report.build = build;
            }
            facts.other_brokers = vec![2];
            let text = text(&facts).0;
            let broker: Vec<&str> = text
                .lines()
                .skip_while(|l| !l.contains("</> dffyd "))
                .collect();
            insta::assert_snapshot!(broker.join("\n"));
        }
    }

    /// Watches the broker holds for this repository count as free for it.
    #[test]
    fn watches_warn_only_when_they_wont_fit() {
        let detail = |used, held| {
            let mut facts = facts();
            facts.inotify_used = Some(used);
            facts.watches_needed = Some(6_345);
            if let Ok(Some(report)) = &mut facts.dffyd {
                report.roots[0].watches = held;
            }
            check(&text(&facts).0, "watch").join("\n")
        };
        insta::assert_snapshot!(
            [detail(125_000, 0), detail(125_000, 6_345), detail(1_000, 0)].join("\n"),
            @r"
        <warn>!</> watch       6,345 needed here · 125,000 of 129,194 in use
          <warn>only 4,194 free: watch mode would poll this repository once a second</>
          <hint>hint:</> raise the limit: sudo sysctl fs.inotify.max_user_watches=524288
        <ok>✓</> watch       6,345 needed here · 125,000 of 129,194 in use · dffyd holds them already
        <ok>✓</> watch       6,345 needed here · 1,000 of 129,194 in use
        "
        );
    }

    /// Off Linux there's no limit to check.
    #[test]
    fn fsevents_needs_no_watches() {
        let facts = Facts {
            inotify: None,
            inotify_used: None,
            watches_needed: None,
            ..facts()
        };
        insta::assert_snapshot!(check(&text(&facts).0, "watch").join("\n"), @"<dim>·</> watch       FSEvents needs no per-directory watches");
    }

    #[test]
    fn a_capped_measurement_says_so() {
        let mut facts = facts();
        if let Some(speed) = &mut facts.speed {
            (speed.changes, speed.capped) = (50_000, true);
            speed.untracked = None;
            (speed.largest, speed.largest_skipped) = (None, true);
        }
        insta::assert_snapshot!(check(&text(&facts).0, "speed").join("\n"));
    }

    /// The JSON's keys, with every value as text shows it; a path with a
    /// quote and a control character stays valid JSON.
    #[test]
    fn the_json_report() {
        let mut facts = facts();
        facts.config_warnings = vec!["line 3: \"colour\"\tunknown\u{1}".to_owned()];
        let (json, failed) = facts.json();
        assert!(!failed);
        insta::assert_snapshot!(json);
        let outside = Facts {
            repo: Err(NOT_A_REPOSITORY.to_owned()),
            speed: None,
            watches_needed: None,
            dffyd: Ok(None),
            cache: None,
            ..facts
        };
        insta::assert_snapshot!("json_outside_a_repository", outside.json().0);
    }
}
