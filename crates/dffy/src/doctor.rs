//! `dffy doctor`: the environment facts that decide whether dffy works well
//! here, and a quick measurement of the repository. Gathering and judging
//! are separate so the report's shape can be tested with fixed facts; the
//! same checks print as text or as one JSON object.

use std::borrow::Cow;
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
use crate::agents::{Place, State};
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
    pub review: Option<ReviewPlugin>,
    /// `None` when not gathered.
    pub agent: Option<AgentFacts>,
    /// dffy's skill in each place agents read skills from: the home
    /// directory's, then the repository's that hold one.
    pub skills: Vec<SkillFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillFacts {
    pub place: Place,
    pub state: Result<State, String>,
}

/// The agent plugin, `dffy-agent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentFacts {
    /// Where its executable is, if it's on PATH; the checks are `None`
    /// when it isn't.
    pub path: Option<PathBuf>,
    /// Its own checks of the agents, from its `doctor`, or why they
    /// couldn't be read.
    pub checks: Option<Result<Vec<crate::plugin::review::Check>, String>>,
}

/// The review plugin `review` picks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewPlugin {
    /// The one it names, or the only one `auto` found.
    One(ReviewFacts),
    /// The names `auto` found when there were several, so it took none.
    Several(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewFacts {
    pub name: String,
    /// Where its executable is, if it's on PATH. The rest is `None` when
    /// it isn't.
    pub path: Option<PathBuf>,
    /// The command it describes, or why it adds none.
    pub command: Option<Result<String, String>>,
    /// Its own checks, from its `doctor`, or why they couldn't be read.
    pub checks: Option<Result<Vec<crate::plugin::review::Check>, String>>,
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
        let plugins = plugins(ctx);
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
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
        let (review, agent) = plugins();
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        Self {
            build: crate::BUILD,
            skills: skills(
                home.as_deref(),
                repo.as_ref().ok().map(|r| r.path.as_path()),
            ),
            home,
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
            multiplexer: multiplexer(env),
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
                // Definitions are kept apart, under a budget of their own.
                let symbols = cache.symbols();
                let usage = |c: &dffy_core::cache::Cache| {
                    c.usage().map_err(|e| format!("{}: {e}", c.dir().display()))
                };
                let (bytes, entries) = usage(cache)?;
                let (symbol_bytes, symbol_entries) = usage(symbols.as_cache())?;
                Ok(CacheUsage {
                    dir: cache.dir().to_owned(),
                    bytes: bytes + symbol_bytes,
                    entries: entries + symbol_entries,
                    budget: ctx.config.cache_size + symbols.budget(),
                })
            }),
            renderers: renderers(&ctx.config.render),
            review,
            agent: Some(agent),
        }
    }

    /// Writes the report as text, in sections, each group of checks on one
    /// row, or with `verbose` each check on its own with its whole detail.
    /// Returns whether any check failed.
    pub(crate) fn render(&self, c: &mut Canvas, verbose: bool) -> bool {
        let mut checks = self.checks();
        checks.sort_by_key(|check| check.section);
        let rows = if verbose {
            checks.iter().map(Row::of).collect()
        } else {
            Row::folded(&checks)
        };
        let width = rows
            .iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        let version = if verbose {
            self.build
        } else {
            self.build.split('+').next().unwrap_or(self.build)
        };
        c.line(Role::FileHeader, &format!("dffy {version}"));
        let mut section = Section::Dffy;
        for row in &rows {
            // The agent plugin's row heads its section, which it runs.
            let heading = !verbose && row.section == Section::Agents && row.label == "agents";
            if row.section != section {
                section = row.section;
                c.push(Role::FileHeader, section.title());
                if !heading {
                    c.end_line();
                }
            }
            if heading {
                // Details quote paths, config values and environment
                // variables.
                let pad = width + 4 - section.title().chars().count();
                c.spaces(Role::Plain, pad);
                c.line(Role::Plain, &escape_lines(&row.detail));
            } else {
                let (symbol, role) = match row.mark {
                    Mark::Ok => ("✓", Role::Ok),
                    Mark::Warn => ("!", Role::Warn),
                    Mark::Fail => ("✗", Role::Error),
                    Mark::Info => ("·", Role::Dim),
                };
                c.push(Role::Plain, "  ");
                c.push(role, symbol);
                c.push_fmt(Role::Plain, format_args!(" {:<width$}", row.label));
                c.line(Role::Plain, &escape_lines(&row.detail));
            }
            for note in &row.notes {
                c.push(Role::Plain, "      ");
                c.line(Role::Warn, &escape_lines(note));
            }
            for hint in &row.hints {
                c.push(Role::Plain, "      ");
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
                    ("name", Json::str(&check.label)),
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
            ("skills", skills_json(&self.skills)),
            (
                "review",
                self.review.as_ref().map_or(Json::Null, review_json),
            ),
            ("agent", self.agent.as_ref().map_or(Json::Null, agent_json)),
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

    /// Every check, in the order `--json` lists them.
    fn checks(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        let mut add = |section, add: &dyn Fn(&mut Vec<Check>)| {
            let from = checks.len();
            add(&mut checks);
            for check in &mut checks[from..] {
                check.section = section;
            }
        };
        add(Section::Dffy, &|c| {
            self.repository(c);
            self.watch(c);
            self.speed(c);
        });
        add(Section::Terminal, &|c| {
            self.terminal(c);
            self.settings(c);
        });
        add(Section::Plugins, &|c| {
            self.renderer_checks(c);
            if let Some(review) = &self.review {
                self.review_checks(review, c);
            }
        });
        add(Section::Agents, &|c| {
            if let Some(agent) = &self.agent {
                self.agent_checks(agent, c);
            }
            self.skill_checks(c);
        });
        add(Section::Dffy, &|c| self.broker(c));
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
            Ok(version) => checks.push(
                Check::new(Mark::Ok, "git", version)
                    .brief(version.trim_start_matches("git version ")),
            ),
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
                checks.push(
                    Check::new(Mark::Info, label, "FSEvents needs no per-directory watches")
                        .brief("FSEvents"),
                );
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
        let all = format!(
            "{}{} {changes} {}",
            thousands(s.changes),
            if s.capped { "+" } else { "" },
            millis(s.all)
        );
        let untracked = match s.untracked {
            Some(walk) => format!("untracked {}", millis(walk)),
            None => "untracked not timed".to_owned(),
        };
        let brief = [&parts[..], &[all.clone(), untracked.clone()]]
            .concat()
            .join(" · ");
        parts.extend([format!("all {all}"), untracked]);
        if let Some((path, took)) = &s.largest {
            parts.push(format!("largest diff {} ({path})", millis(*took)));
        } else if s.largest_skipped {
            parts.push("largest diff not timed".to_owned());
        }
        let mut check = Check::new(Mark::Info, "speed", parts.join(" · ")).brief(brief);
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
            Ok(None) => checks.push(
                Check::new(
                    Mark::Info,
                    label,
                    "not running; the first viewer or listening session starts it",
                )
                .brief("not running"),
            ),
            Ok(Some(report)) => {
                let subscribers: u32 = report.roots.iter().map(|root| root.subscribers).sum();
                let running = format!(
                    "running (pid {}), {} root(s), {subscribers} subscriber(s)",
                    report.pid,
                    report.roots.len()
                );
                let mut check = if self.current(report) {
                    Check::new(Mark::Ok, label, running)
                        .brief(format!("running (pid {})", report.pid))
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
                    .brief(format!(
                        "running (pid {}), another dffy's build",
                        report.pid
                    ))
                    .hint(
                        "dffy daemon stop; each viewer and session starts dffyd from the dffy it \
                         runs, so reopen those of another build",
                    )
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
        let brief: Vec<String> = [
            Some(self.term.clone().unwrap_or_else(|| "TERM unset".to_owned())),
            self.colorterm.clone(),
            self.term_program.clone(),
            Some(match (self.stdout_tty, self.width) {
                (true, Some(w)) => format!("{w} columns"),
                (true, None) => "width unknown".to_owned(),
                (false, _) => "not a terminal".to_owned(),
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
        checks.push(Check::new(mark, "terminal", terminal.join(", ")).brief(brief.join(" · ")));
        checks.push(match self.multiplexer {
            Some(name) => Check::new(
                Mark::Ok,
                "multiplexer",
                format!(
                    "{name}; dffy's one query (the background color) falls back to dark if it isn't relayed"
                ),
            )
            .brief(name),
            None => Check::new(Mark::Info, "multiplexer", "none detected").brief("none"),
        });
        if self.multiplexer == Some("tmux") {
            let label = "copy";
            checks.push(match self.tmux_clipboard.as_deref() {
                Some("on") => Check::new(
                    Mark::Ok,
                    label,
                    "tmux passes copies (y, Y) to the terminal: set-clipboard is on",
                )
                .brief("tmux set-clipboard on"),
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
        let on = if self.color { "on" } else { "off" };
        checks.push(
            Check::new(
                Mark::Ok,
                "color",
                format!(
                    "{on}; the default theme uses the 16 ANSI colors{}",
                    if truecolor {
                        ", truecolor themes work too"
                    } else {
                        ""
                    }
                ),
            )
            .brief(if truecolor {
                format!("{on} · 16-color theme, truecolor works")
            } else {
                format!("{on} · 16-color theme")
            }),
        );
    }

    fn settings(&self, checks: &mut Vec<Check>) {
        let path = self.config_path.as_ref().map_or_else(
            || "no config directory".to_owned(),
            |p| p.display().to_string(),
        );
        let short = self
            .config_path
            .as_ref()
            .map_or_else(|| "no config directory".to_owned(), |p| self.short(p));
        if !self.config_found {
            checks.push(
                Check::new(
                    Mark::Info,
                    "config",
                    format!("{path} (not present; using defaults)"),
                )
                .brief(format!("{short} (not present)")),
            );
        } else if self.config_warnings.is_empty() {
            checks.push(Check::new(Mark::Ok, "config", path).brief(short));
        } else {
            let count = self.config_warnings.len();
            let mut check = Check::new(Mark::Warn, "config", format!("{path}: {count} problem(s)"))
                .brief(format!("{short}: {count} problem(s)"));
            check.notes.clone_from(&self.config_warnings);
            checks.push(check);
        }
        checks.push(
            Check::new(Mark::Ok, "theme", &self.theme)
                .group("config")
                .brief(format!("theme {}", self.theme)),
        );
        checks.push(
            Check::new(
                Mark::Ok,
                "pager",
                self.pager
                    .as_deref()
                    .unwrap_or("none; output goes straight to the terminal"),
            )
            .group("config")
            .brief(
                self.pager
                    .as_ref()
                    .map_or_else(|| "no pager".to_owned(), |p| format!("pager {p}")),
            ),
        );
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
            )
            .brief(format!(
                "{} · {} of {}",
                self.short(&usage.dir),
                megabytes(usage.bytes),
                megabytes(usage.budget)
            )),
            Some(Err(e)) => Check::new(Mark::Warn, "cache", format!("can't read: {e}"))
                .hint("fix its permissions or set cache = false; dffy works without it"),
        });
    }

    /// A line per renderer the config sets, folded into one `render` row
    /// that lists each renderer found once, with its extensions.
    fn renderer_checks(&self, checks: &mut Vec<Check>) {
        let mut found: Vec<(&str, Vec<&str>)> = Vec::new();
        for (ext, name, at) in &self.renderers {
            if at.is_some() {
                match found.iter_mut().find(|(n, _)| n == name) {
                    Some((_, exts)) => exts.push(ext),
                    None => found.push((name, vec![ext])),
                }
            }
        }
        let mut brief = Some(
            found
                .iter()
                .map(|(name, exts)| {
                    let exts: Vec<String> = exts.iter().map(|e| format!(".{e}")).collect();
                    format!("{name} {}", exts.join(" "))
                })
                .collect::<Vec<_>>()
                .join(" · "),
        );
        for (ext, name, at) in &self.renderers {
            let check = match at {
                Some(path) => {
                    let check = Check::new(
                        Mark::Ok,
                        "renderer",
                        format!(".{ext} → {name}: {}", self.short(path)),
                    );
                    match brief.take() {
                        Some(all) => check.brief(all),
                        None => Check {
                            brief: Brief::Folded,
                            ..check
                        },
                    }
                }
                None => Check::new(
                    Mark::Warn,
                    "renderer",
                    format!(
                        ".{ext} → {name}: {}{name} isn't on PATH",
                        crate::plugin::PREFIX
                    ),
                )
                .hint("install it, or see docs/plugins.md; files of this type show as lines"),
            };
            checks.push(check.group("render"));
        }
    }

    /// The agent plugin's line, the agents section's heading, then a line
    /// per check of its own, those of an agent folded into its row: a
    /// check's group is its name's first word.
    fn agent_checks(&self, agent: &AgentFacts, checks: &mut Vec<Check>) {
        let program = crate::plugin::agent::PROGRAM;
        let Some(path) = &agent.path else {
            checks.push(
                Check::new(Mark::Info, "agent", format!("{program} isn't on PATH"))
                    .group("agents")
                    .hint("asking an agent about a thread needs it; see docs/plugins.md"),
            );
            return;
        };
        let at = format!("{program}: {}", self.short(path));
        let brief = format!("{program} {}", self.short(path));
        checks.push(
            match &agent.checks {
                Some(Err(why)) => {
                    Check::new(Mark::Warn, "agent", at).note(format!("its checks: {why}"))
                }
                _ => Check::new(Mark::Ok, "agent", at),
            }
            .group("agents")
            .brief(brief),
        );
        for own in agent.checks.iter().flatten().flatten() {
            // Labels aren't escaped when drawn, and these come from the
            // plugin.
            let name = escape(own.name.as_bytes());
            let group = name.split(' ').next().unwrap_or_default().to_owned();
            checks.push(own_check(own, format!("agent {name}"), group));
        }
    }

    /// A line per skill location: dffy's skill there is current,
    /// outdated, someone else's or missing. The repository's own show
    /// only when there's one.
    fn skill_checks(&self, checks: &mut Vec<Check>) {
        // Each state with the places in it; a place that can't be read is
        // its error alone.
        let mut parts: Vec<(String, Vec<String>)> = Vec::new();
        for skill in &self.skills {
            let file = self.short(&skill.place.file);
            let dir = skill
                .place
                .file
                .parent()
                .and_then(Path::parent)
                .map_or_else(|| file.clone(), |d| self.short(d));
            let readers = skill.place.readers();
            let check = match &skill.state {
                Ok(state) => {
                    let detail = format!("{file} ({readers}): {}", state.name());
                    let check = match state {
                        State::Current => Check::new(Mark::Ok, "skill", detail),
                        State::Outdated => Check::new(Mark::Warn, "skill", detail)
                            .hint("run dffy agents setup to update it"),
                        State::Foreign => Check::new(Mark::Info, "skill", detail)
                            .note("a dffy skill dffy didn't write"),
                        State::Missing => Check::new(Mark::Info, "skill", detail),
                    };
                    match parts.iter_mut().find(|(s, _)| s == state.name()) {
                        Some((_, dirs)) => dirs.push(dir),
                        None => parts.push((state.name().to_owned(), vec![dir])),
                    }
                    check
                }
                Err(e) => {
                    parts.push((e.clone(), Vec::new()));
                    Check::new(Mark::Warn, "skill", e.clone())
                }
            };
            checks.push(Check {
                brief: Brief::Folded,
                ..check
            });
        }
        let Some(first) = checks.iter_mut().find(|c| c.label == "skill") else {
            return;
        };
        if parts
            .iter()
            .any(|(state, _)| state == State::Missing.name())
        {
            first
                .hints
                .push("dffy agents setup teaches agents to use dffy".to_owned());
        }
        first.brief = Brief::Text(
            parts
                .iter()
                .map(|(state, dirs)| match &dirs[..] {
                    [] => state.clone(),
                    dirs => format!("{state} in {}", dirs.join(", ")),
                })
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }

    /// The review plugin's line, then a line per check of its own.
    fn review_checks(&self, review: &ReviewPlugin, checks: &mut Vec<Check>) {
        let review = match review {
            ReviewPlugin::One(review) => review,
            ReviewPlugin::Several(names) => {
                let found = format!(
                    "review plugins on PATH: {}, so none is used",
                    names.join(", ")
                );
                checks.push(
                    Check::new(Mark::Warn, "review", found).hint("pick one with review = <name>"),
                );
                return;
            }
        };
        let program = format!("{}{}", crate::plugin::REVIEW_PREFIX, review.name);
        let Some(path) = &review.path else {
            checks.push(
                Check::new(Mark::Warn, "review", format!("{program} isn't on PATH"))
                    .hint("install it, or see docs/plugins.md; until then the review keys say so"),
            );
            return;
        };
        let at = format!("{program}: {}", self.short(path));
        let mut check = match &review.command {
            Some(Ok(command)) => {
                Check::new(Mark::Ok, "review", format!("{at}, adds dffy {command}"))
                    .brief(&review.name)
            }
            Some(Err(why)) => Check::new(Mark::Warn, "review", format!("{at}, adds no command"))
                .note(why.clone())
                .hint("see docs/plugins.md; a plugin names its command with describe"),
            None => Check::new(Mark::Ok, "review", at).brief(&review.name),
        };
        if let Some(Err(why)) = &review.checks {
            check = check.note(format!("its checks: {why}"));
        }
        checks.push(check);
        for own in review.checks.iter().flatten().flatten() {
            // Labels aren't escaped when drawn, and these come from the
            // plugin.
            let label = format!("review {}", escape(own.name.as_bytes()));
            checks.push(own_check(own, label, "review"));
        }
    }
}

/// One of a plugin's own checks, folded into `group`'s row by its brief
/// when it passed; a failed one warns, with its whole detail, and one that
/// only notes something is a note.
fn own_check(
    own: &crate::plugin::review::Check,
    label: String,
    group: impl Into<Cow<'static, str>>,
) -> Check {
    let mark = match (own.ok, own.info) {
        (false, _) => Mark::Warn,
        (true, true) => Mark::Info,
        (true, false) => Mark::Ok,
    };
    let check = Check::new(mark, label, own.detail.clone()).group(group);
    match &own.brief {
        Some(brief) if own.ok => check.brief(brief.clone()),
        _ => check,
    }
}

/// Starts asking the review plugin the config picks, if any, and the agent
/// plugin, side by side on threads of their own: their checks wait on
/// programs of theirs, a few seconds at most, so they run beside dffy's
/// own and doctor takes the slowest. What's returned waits for them.
fn plugins(ctx: &Ctx) -> impl FnOnce() -> (Option<ReviewPlugin>, AgentFacts) + use<> {
    fn joined<T>(thread: std::thread::JoinHandle<T>) -> T {
        (thread.join()).unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }
    let (review, cache, agent) = (
        ctx.config.review.clone(),
        ctx.cache.clone(),
        ctx.config.agent.clone(),
    );
    let agent = std::thread::spawn(move || self::agent(agent));
    let review = std::thread::spawn(move || review_plugin(&review, cache.as_ref()));
    move || (joined(review), joined(agent))
}

/// The review plugin `review` picks, asked now. Under `auto` PATH is listed
/// again, replacing the cached listing, as the plugin's description is.
fn review_plugin(
    review: &crate::plugin::Review,
    cache: Option<&dffy_core::cache::Cache>,
) -> Option<ReviewPlugin> {
    use crate::plugin::{REVIEW_PREFIX, Review};
    let name = match review {
        Review::Off => return None,
        Review::Named(name) => name.clone(),
        Review::Auto => {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut found = crate::plugin::installed(REVIEW_PREFIX, &path, cache, true);
            match found.len() {
                0 => return None,
                1 => found.remove(0),
                _ => return Some(ReviewPlugin::Several(found)),
            }
        }
    };
    Some(ReviewPlugin::One(self::review(&name, cache)))
}

/// The review plugin `name`: the command it describes and its own checks,
/// both asked now, side by side. What it describes replaces the cached
/// answer, so a failure kept from before is asked again.
fn review(name: &str, cache: Option<&dffy_core::cache::Cache>) -> ReviewFacts {
    let found = crate::plugin::find(crate::plugin::REVIEW_PREFIX, name);
    let (command, checks) = std::thread::scope(|scope| {
        let checks =
            (found.as_ref()).map(|program| scope.spawn(|| crate::plugin::review::doctor(program)));
        let command = (found.as_ref())
            .map(|_| crate::describe_review(name, cache, true).map(|d| d.naming.command));
        let checks = checks
            .map(|checks| (checks.join()).unwrap_or_else(|panic| std::panic::resume_unwind(panic)));
        (command, checks)
    });
    ReviewFacts {
        name: name.to_owned(),
        command,
        checks,
        path: found.map(|p| p.path),
    }
}

/// The agent plugin, found on PATH, and its own checks, asked now: a
/// process started for them alone, which probes the agents again.
/// `config` is the config's `agent`.
fn agent(config: Option<String>) -> AgentFacts {
    let found = crate::plugin::agent::find();
    AgentFacts {
        checks: (found.as_ref()).map(|program| crate::plugin::agent::doctor(program, config)),
        path: found.map(|p| p.path),
    }
}

/// The multiplexer dffy runs in, from the variables `env` reads.
fn multiplexer(env: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    if env("TMUX").is_some() {
        Some("tmux")
    } else if env("ZELLIJ").is_some() {
        Some("zellij")
    } else if env("STY").is_some() {
        Some("screen")
    } else if env("TERM_PROGRAM").is_some_and(|p| p == "herdr") {
        // herdr names itself in TERM_PROGRAM, and sets HERDR_ENV and
        // other HERDR_ variables too.
        Some("herdr")
    } else {
        None
    }
}

/// dffy's skill under `home`, and under the repository's root `worktree`
/// where it's there at all.
fn skills(home: Option<&Path>, worktree: Option<&Path>) -> Vec<SkillFacts> {
    let judged = |base: &Path| {
        crate::agents::places(base, &[])
            .into_iter()
            .map(|place| SkillFacts {
                state: crate::agents::state(&place.file),
                place,
            })
            .collect::<Vec<_>>()
    };
    let mut skills = home.map(judged).unwrap_or_default();
    if let Some(worktree) = worktree.filter(|w| Some(*w) != home) {
        skills.extend(
            judged(worktree)
                .into_iter()
                .filter(|s| s.state != Ok(State::Missing)),
        );
    }
    skills
}

fn skills_json(skills: &[SkillFacts]) -> Json {
    Json::Array(
        skills
            .iter()
            .map(|skill| {
                Json::Object(vec![
                    ("path", Json::path(&skill.place.file)),
                    (
                        "agents",
                        Json::Array(
                            (skill.place.agents.iter())
                                .map(|a| Json::str(a.id()))
                                .collect(),
                        ),
                    ),
                    (
                        "status",
                        skill
                            .state
                            .as_ref()
                            .map_or(Json::Null, |s| Json::str(s.name())),
                    ),
                    ("error", Json::error(skill.state.as_ref())),
                ])
            })
            .collect(),
    )
}

fn agent_json(agent: &AgentFacts) -> Json {
    let checks = agent.checks.as_ref();
    Json::Object(vec![
        ("path", agent.path.as_ref().map_or(Json::Null, Json::path)),
        (
            "checks",
            checks
                .and_then(|r| r.as_ref().ok())
                .map_or(Json::Null, |c| checks_json(c)),
        ),
        (
            "checks_error",
            checks.map_or(Json::Null, |r| Json::error(r.as_ref())),
        ),
    ])
}

fn checks_json(checks: &[crate::plugin::review::Check]) -> Json {
    Json::Array(
        checks
            .iter()
            .map(|c| {
                Json::Object(vec![
                    ("name", Json::str(&c.name)),
                    ("ok", Json::Bool(c.ok)),
                    ("detail", Json::str(&c.detail)),
                ])
            })
            .collect(),
    )
}

/// The review plugin in use, or the several `auto` found under `installed`
/// with the rest null.
fn review_json(review: &ReviewPlugin) -> Json {
    let (review, installed) = match review {
        ReviewPlugin::One(review) => (Some(review), Json::Null),
        ReviewPlugin::Several(names) => (None, Json::Array(names.iter().map(Json::str).collect())),
    };
    let command = review.and_then(|r| r.command.as_ref());
    let checks = review.and_then(|r| r.checks.as_ref());
    let listed = checks
        .and_then(|r| r.as_ref().ok())
        .map_or(Json::Null, |checks| checks_json(checks));
    Json::Object(vec![
        ("name", review.map_or(Json::Null, |r| Json::str(&r.name))),
        (
            "path",
            review
                .and_then(|r| r.path.as_ref())
                .map_or(Json::Null, Json::path),
        ),
        (
            "command",
            command
                .and_then(|r| r.as_ref().ok())
                .map_or(Json::Null, Json::str),
        ),
        (
            "command_error",
            command.map_or(Json::Null, |r| Json::error(r.as_ref())),
        ),
        ("checks", listed),
        (
            "checks_error",
            checks.map_or(Json::Null, |r| Json::error(r.as_ref())),
        ),
        ("installed", installed),
    ])
}

/// Each configured renderer with its executable, if it's on PATH.
fn renderers(render: &crate::plugin::Renderers) -> Vec<(String, String, Option<PathBuf>)> {
    render
        .pairs()
        .iter()
        .map(|(ext, name)| {
            let found = crate::plugin::find(crate::plugin::PREFIX, name).map(|p| p.path);
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
    /// The one of the two a folded row shows: a failure over a warning
    /// over a note over a pass, so a row isn't marked passed when part of
    /// it is only noted, as a config file that isn't there.
    fn worst(self, other: Self) -> Self {
        let rank = |mark| match mark {
            Self::Ok => 0,
            Self::Info => 1,
            Self::Warn => 2,
            Self::Fail => 3,
        };
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Info => "info",
        }
    }
}

/// Where a check shows in the text report, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    /// Under the `dffy <version>` heading.
    Dffy,
    Terminal,
    Plugins,
    Agents,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::Dffy => "dffy",
            Self::Terminal => "terminal",
            Self::Plugins => "plugins",
            Self::Agents => "agents",
        }
    }
}

/// What a check says in its row of the folded report.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Brief {
    /// Its detail.
    Detail,
    Text(String),
    /// Nothing: another check of its row says it for both.
    Folded,
}

/// One line of the report, with the lines under it. `--json` and
/// `--verbose` give each check its own line; the text report folds the
/// checks of a `group` into one row, their briefs joined.
struct Check {
    mark: Mark,
    /// dffy's own, or `review <name>` for one of the review plugin's.
    label: Cow<'static, str>,
    detail: String,
    /// Indented lines in the warning color.
    notes: Vec<String>,
    hints: Vec<String>,
    section: Section,
    /// The folded row's label; `agents` is the agents section's heading.
    group: Cow<'static, str>,
    brief: Brief,
}

impl Check {
    fn new(mark: Mark, label: impl Into<Cow<'static, str>>, detail: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            mark,
            group: label.clone(),
            label,
            detail: detail.into(),
            notes: Vec::new(),
            hints: Vec::new(),
            section: Section::Dffy,
            brief: Brief::Detail,
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

    fn brief(mut self, text: impl Into<String>) -> Self {
        self.brief = Brief::Text(text.into());
        self
    }

    fn group(mut self, group: impl Into<Cow<'static, str>>) -> Self {
        self.group = group.into();
        self
    }
}

/// A line of the text report: a check, or the checks of a group folded.
struct Row<'a> {
    mark: Mark,
    label: &'a str,
    detail: Cow<'a, str>,
    notes: Vec<&'a str>,
    hints: Vec<&'a str>,
    section: Section,
}

impl<'a> Row<'a> {
    fn of(check: &'a Check) -> Self {
        Self {
            mark: check.mark,
            label: &check.label,
            detail: Cow::Borrowed(&check.detail),
            notes: check.notes.iter().map(String::as_str).collect(),
            hints: check.hints.iter().map(String::as_str).collect(),
            section: check.section,
        }
    }

    /// `checks`' groups, each one row in the order it first comes: the
    /// worst mark, the briefs joined, every note and hint.
    fn folded(checks: &'a [Check]) -> Vec<Self> {
        let mut rows: Vec<Self> = Vec::new();
        for check in checks {
            let brief = match &check.brief {
                Brief::Detail => Some(check.detail.as_str()),
                Brief::Text(text) => Some(text.as_str()),
                Brief::Folded => None,
            };
            let at = rows
                .iter()
                .position(|r| r.label == check.group && r.section == check.section);
            let Some(at) = at else {
                rows.push(Self {
                    mark: check.mark,
                    label: &check.group,
                    detail: Cow::Borrowed(brief.unwrap_or_default()),
                    notes: check.notes.iter().map(String::as_str).collect(),
                    hints: check.hints.iter().map(String::as_str).collect(),
                    section: check.section,
                });
                continue;
            };
            let row = &mut rows[at];
            row.mark = row.mark.worst(check.mark);
            if let Some(brief) = brief.filter(|b| !b.is_empty()) {
                let detail = row.detail.to_mut();
                if !detail.is_empty() {
                    detail.push_str(" · ");
                }
                detail.push_str(brief);
            }
            row.notes.extend(check.notes.iter().map(String::as_str));
            row.hints.extend(check.hints.iter().map(String::as_str));
        }
        rows
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
            review: None,
            agent: None,
            skills: Vec::new(),
        }
    }

    fn text(facts: &Facts) -> (String, bool) {
        let mut c = Canvas::default();
        let failed = facts.render(&mut c, true);
        (c.markup(), failed)
    }

    fn folded(facts: &Facts) -> String {
        let mut c = Canvas::default();
        facts.render(&mut c, false);
        c.markup()
    }

    /// The lines of the section of `text` headed `title`.
    fn section<'a>(text: &'a str, title: &str) -> Vec<&'a str> {
        let mut lines = text
            .lines()
            .skip_while(|l| !l.contains(&format!(">{title}</>")));
        let heading = lines.next().into_iter();
        heading
            .chain(lines.take_while(|l| l.starts_with(' ')))
            .collect()
    }

    /// The lines of `text` from the one labelled `label`, with what's
    /// indented under it.
    fn check<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
        let labelled = |l: &str| {
            l.split_once("</> ")
                .and_then(|(_, rest)| rest.strip_prefix(label))
                .is_some_and(|rest| rest.starts_with("  "))
        };
        let mut lines = text.lines().skip_while(|l| !labelled(l));
        let first = lines.next().into_iter();
        first
            .chain(lines.take_while(|l| l.starts_with("      ")))
            .collect()
    }

    #[test]
    fn healthy_environment() {
        let (text, failed) = text(&facts());
        assert!(!failed);
        insta::assert_snapshot!(text);
        insta::assert_snapshot!("healthy_environment_folded", folded(&facts()));
    }

    /// A line per place in `--verbose` and `--json`; one row in the text,
    /// the places of each state together, with a hint while one is missing
    /// and how to update one that's outdated.
    #[test]
    fn the_skill_row_says_each_place_by_its_state() {
        let skills = |states: [Result<State, String>; 2]| {
            let places = crate::agents::places(Path::new("/home/u"), &[]);
            let facts = Facts {
                skills: places
                    .into_iter()
                    .zip(states)
                    .map(|(place, state)| SkillFacts { place, state })
                    .collect(),
                ..facts()
            };
            let json = facts.json().0;
            let at = json.find("\"skills\":").expect("the skills key");
            let end = json.find(",\"review\":").expect("the review key");
            [
                section(&folded(&facts), "agents").join("\n"),
                check(&text(&facts).0, "skill").join("\n"),
                json[at..end].to_owned(),
            ]
            .join("\n")
        };
        insta::assert_snapshot!(
            [
                skills([Ok(State::Current), Ok(State::Current)]),
                skills([Ok(State::Outdated), Ok(State::Missing)]),
                skills([Ok(State::Foreign), Err("can't read it: denied".to_owned())]),
                skills([Err("can't read it: denied".to_owned()), Ok(State::Current)]),
            ]
            .join("\n\n"),
            @r#"
            <file-header>agents</>
              <ok>✓</> skill        current in ~/.claude/skills, ~/.agents/skills
              <ok>✓</> skill        ~/.claude/skills/dffy/SKILL.md (Claude Code): current
            "skills":[{"path":"/home/u/.claude/skills/dffy/SKILL.md","agents":["claude"],"status":"current","error":null},{"path":"/home/u/.agents/skills/dffy/SKILL.md","agents":["codex","pi","amp"],"status":"current","error":null}]

            <file-header>agents</>
              <warn>!</> skill        outdated in ~/.claude/skills · missing in ~/.agents/skills
                  <hint>hint:</> run dffy agents setup to update it
                  <hint>hint:</> dffy agents setup teaches agents to use dffy
              <warn>!</> skill        ~/.claude/skills/dffy/SKILL.md (Claude Code): outdated
                  <hint>hint:</> run dffy agents setup to update it
                  <hint>hint:</> dffy agents setup teaches agents to use dffy
            "skills":[{"path":"/home/u/.claude/skills/dffy/SKILL.md","agents":["claude"],"status":"outdated","error":null},{"path":"/home/u/.agents/skills/dffy/SKILL.md","agents":["codex","pi","amp"],"status":"missing","error":null}]

            <file-header>agents</>
              <warn>!</> skill        foreign in ~/.claude/skills · can't read it: denied
                  <warn>a dffy skill dffy didn't write</>
              <dim>·</> skill        ~/.claude/skills/dffy/SKILL.md (Claude Code): foreign
                  <warn>a dffy skill dffy didn't write</>
            "skills":[{"path":"/home/u/.claude/skills/dffy/SKILL.md","agents":["claude"],"status":"foreign","error":null},{"path":"/home/u/.agents/skills/dffy/SKILL.md","agents":["codex","pi","amp"],"status":null,"error":"can't read it: denied"}]

            <file-header>agents</>
              <warn>!</> skill        can't read it: denied · current in ~/.agents/skills
              <warn>!</> skill        can't read it: denied
            "skills":[{"path":"/home/u/.claude/skills/dffy/SKILL.md","agents":["claude"],"status":null,"error":"can't read it: denied"},{"path":"/home/u/.agents/skills/dffy/SKILL.md","agents":["codex","pi","amp"],"status":"current","error":null}]
            "#
        );
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
        insta::assert_snapshot!(check(&text, "repository").join("\n"), @"  <dim>·</> repository   not in a git repository (-C <path>)");
        insta::assert_snapshot!(check(&text, "watch").join("\n"), @"  <ok>✓</> watch        5,259 of 129,194 in use");
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
            @r#"
            <ok>✓</> repository   ~/src/linux · main (no commits yet) · 265 staged
            <ok>✓</> repository   ~/src/linux · detached at 80f116f · nothing uncommitted
            <ok>✓</> repository   ~/src/linux · main · 50,000 staged (counting stopped at 50,000 changes)
            "#
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
            @r#"
            <warn>!</> watch        6,345 needed here · 125,000 of 129,194 in use
                <warn>only 4,194 free: watch mode would poll this repository once a second</>
                <hint>hint:</> raise the limit: sudo sysctl fs.inotify.max_user_watches=524288
            <ok>✓</> watch        6,345 needed here · 125,000 of 129,194 in use · dffyd holds them already
            <ok>✓</> watch        6,345 needed here · 1,000 of 129,194 in use
            "#
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
        insta::assert_snapshot!(check(&text(&facts).0, "watch").join("\n"), @"  <dim>·</> watch        FSEvents needs no per-directory watches");
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

    /// The review plugin set, found or not, with the command it adds or
    /// why it adds none, then its own checks; nothing without one.
    #[test]
    fn the_review_plugin_is_listed_with_its_command_and_checks() {
        use crate::plugin::review::Check as Own;
        let review = |found: bool, command: Result<&str, &str>, checks: Result<&[Own], &str>| {
            let facts = Facts {
                review: Some(ReviewPlugin::One(ReviewFacts {
                    name: "github".to_owned(),
                    path: found.then(|| PathBuf::from("/home/u/bin/dffy-review-github")),
                    command: found.then(|| command.map(str::to_owned).map_err(str::to_owned)),
                    checks: found.then(|| checks.map(<[Own]>::to_vec).map_err(str::to_owned)),
                })),
                ..facts()
            };
            let json = facts.json().0;
            let at = json.find("\"review\":").expect("the review key");
            let end = json.find(",\"agent\":").expect("the agent key");
            let shown = folded(&facts);
            [
                section(&shown, "plugins").join("\n"),
                json[at..end].to_owned(),
            ]
            .join("\n")
        };
        let own = [
            Own {
                name: "gh".to_owned(),
                ok: true,
                detail: "gh version 2.62.0".to_owned(),
                brief: Some("gh 2.62.0".to_owned()),
                info: false,
            },
            Own {
                name: "auth".to_owned(),
                ok: false,
                detail: "gh isn't logged in to github.com; run `gh auth login`".to_owned(),
                brief: None,
                info: false,
            },
        ];
        insta::assert_snapshot!(
            [
                review(true, Ok("pr"), Ok(&own)),
                review(true, Err("its command 'diff' is one of dffy's own"), Err("it doesn't know doctor; it may be older than this dffy")),
                review(false, Ok("pr"), Ok(&own)),
            ]
            .join("\n\n"),
            @r#"
            <file-header>plugins</>
              <warn>!</> render       markdown .md · .csv → csv: dffy-render-csv isn't on PATH
                  <hint>hint:</> install it, or see docs/plugins.md; files of this type show as lines
              <warn>!</> review       github · gh 2.62.0 · gh isn't logged in to github.com; run `gh auth login`
            "review":{"name":"github","path":"/home/u/bin/dffy-review-github","command":"pr","command_error":null,"checks":[{"name":"gh","ok":true,"detail":"gh version 2.62.0"},{"name":"auth","ok":false,"detail":"gh isn't logged in to github.com; run `gh auth login`"}],"checks_error":null,"installed":null}

            <file-header>plugins</>
              <warn>!</> render       markdown .md · .csv → csv: dffy-render-csv isn't on PATH
                  <hint>hint:</> install it, or see docs/plugins.md; files of this type show as lines
              <warn>!</> review       dffy-review-github: ~/bin/dffy-review-github, adds no command
                  <warn>its command 'diff' is one of dffy's own</>
                  <warn>its checks: it doesn't know doctor; it may be older than this dffy</>
                  <hint>hint:</> see docs/plugins.md; a plugin names its command with describe
            "review":{"name":"github","path":"/home/u/bin/dffy-review-github","command":null,"command_error":"its command 'diff' is one of dffy's own","checks":null,"checks_error":"it doesn't know doctor; it may be older than this dffy","installed":null}

            <file-header>plugins</>
              <warn>!</> render       markdown .md · .csv → csv: dffy-render-csv isn't on PATH
                  <hint>hint:</> install it, or see docs/plugins.md; files of this type show as lines
              <warn>!</> review       dffy-review-github isn't on PATH
                  <hint>hint:</> install it, or see docs/plugins.md; until then the review keys say so
            "review":{"name":"github","path":null,"command":null,"command_error":null,"checks":null,"checks_error":null,"installed":null}
            "#
        );
        assert_eq!(check(&text(&facts()).0, "review"), Vec::<&str>::new());
        assert!(facts().json().0.contains("\"review\":null"));
        let several = Facts {
            review: Some(ReviewPlugin::Several(vec![
                "one".to_owned(),
                "two".to_owned(),
            ])),
            ..facts()
        };
        insta::assert_snapshot!(check(&text(&several).0, "review").join("\n"), @r"
        <warn>!</> review       review plugins on PATH: one, two, so none is used
            <hint>hint:</> pick one with review = <name>
        ");
        assert!(
            several
                .json()
                .0
                .contains(r#""checks_error":null,"installed":["one","two"]}"#)
        );
    }

    /// The agent plugin, found or not, then its own checks of the agents;
    /// a check that failed warns.
    #[test]
    fn the_agent_plugin_is_listed_with_its_checks() {
        use crate::plugin::review::Check as Own;
        let agent = |path: Option<&str>, checks: Option<Result<Vec<Own>, String>>| {
            let facts = Facts {
                agent: Some(AgentFacts {
                    path: path.map(PathBuf::from),
                    checks,
                }),
                ..facts()
            };
            let json = facts.json().0;
            let at = json.find("\"agent\":").expect("the agent key");
            let shown = folded(&facts);
            [
                section(&shown, "agents").join("\n"),
                json[at..].trim_end().to_owned(),
            ]
            .join("\n")
        };
        let own = vec![
            Own {
                name: "claude".to_owned(),
                ok: true,
                detail: "2.1.329 (Claude Code)".to_owned(),
                brief: Some("2.1.329".to_owned()),
                info: false,
            },
            Own {
                name: "claude auth".to_owned(),
                ok: false,
                detail: "not logged in; run `claude auth login`".to_owned(),
                brief: None,
                info: false,
            },
        ];
        insta::assert_snapshot!(
            [
                agent(Some("/home/u/bin/dffy-agent"), Some(Ok(own))),
                agent(Some("/home/u/bin/dffy-agent"), Some(Err("its doctor didn't answer in 3s".to_owned()))),
                agent(None, None),
            ]
            .join("\n\n"),
            @r#"
            <file-header>agents</>           dffy-agent ~/bin/dffy-agent
              <warn>!</> claude       2.1.329 · not logged in; run `claude auth login`
            "agent":{"path":"/home/u/bin/dffy-agent","checks":[{"name":"claude","ok":true,"detail":"2.1.329 (Claude Code)"},{"name":"claude auth","ok":false,"detail":"not logged in; run `claude auth login`"}],"checks_error":null}}

            <file-header>agents</>           dffy-agent ~/bin/dffy-agent
                  <warn>its checks: its doctor didn't answer in 3s</>
            "agent":{"path":"/home/u/bin/dffy-agent","checks":null,"checks_error":"its doctor didn't answer in 3s"}}

            <file-header>agents</>           dffy-agent isn't on PATH
                  <hint>hint:</> asking an agent about a thread needs it; see docs/plugins.md
            "agent":{"path":null,"checks":null,"checks_error":null}}
            "#
        );
    }

    /// A row that only notes something, a missing config file or an agent
    /// that isn't installed, shows `·`, though what's folded with it passed.
    #[test]
    fn a_note_outranks_a_pass_in_a_folded_row() {
        use crate::plugin::review::Check as Own;
        let facts = Facts {
            config_found: false,
            agent: Some(AgentFacts {
                path: Some(PathBuf::from("/home/u/bin/dffy-agent")),
                checks: Some(Ok(vec![Own {
                    name: "codex".to_owned(),
                    ok: true,
                    detail: "not installed".to_owned(),
                    brief: None,
                    info: true,
                }])),
            }),
            ..facts()
        };
        let shown = folded(&facts);
        insta::assert_snapshot!(
            [check(&shown, "config"), check(&shown, "codex")].concat().join("\n"),
            @r"
          <dim>·</> config       ~/.config/dffy/config (not present) · theme default · pager less
          <dim>·</> codex        not installed
        "
        );
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
