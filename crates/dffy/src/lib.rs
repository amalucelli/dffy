//! The `dffy` binary. It's a library only so benchmarks can reach the
//! renderer; [`main`] is the whole program.

mod cli;
mod config;
mod daemon;
mod docs;
mod doctor;
mod json;
mod keys;
mod mcp;
mod no_index;
mod output;
mod pager;
mod plugin;
mod print;
mod render;
mod review;
mod state;
mod term;
mod theme;
mod tui;

#[doc(hidden)]
pub mod bench;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use clap::{ColorChoice, CommandFactory, FromArgMatches};
use dffy_core::cache::Cache;
use dffy_core::git;
use dffy_core::highlight::Registry;

use cli::{Cli, Command, CommentsAction, CommonArgs, DaemonAction, DiffArgs, ShowArgs, ViewArgs};
use config::{Config, DefaultDiff, Indicators, ModifiedLines, Paging, View, Warning};
use output::{Output, Term};
use render::{Canvas, Layout};
use theme::{Role, Theme};

/// Exit status for usage and git errors, as git uses.
const FATAL: u8 = 128;

/// Exit status after Ctrl-C, as a shell reports a process killed by SIGINT.
const INTERRUPTED: u8 = 130;

/// The version and a hash of the sources this binary is built from.
pub(crate) const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("DFFY_SOURCE_HASH"));

/// The grammars dffy highlights and syntax-diffs with: the core's built-in
/// set, built on first use and kept for the process, so each language's
/// query is compiled at most once. Entry points fetch it and pass it down.
pub(crate) fn grammars() -> &'static Registry {
    static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);
    &GRAMMARS
}

#[derive(Debug)]
pub(crate) enum Error {
    /// The reader of our output went away; exit quietly with success.
    BrokenPipe,
    /// Ctrl-C while a pager was showing our output, or before `dffy pr`'s
    /// viewer opened; exit quietly.
    Interrupted,
    Fatal {
        message: String,
        hint: Option<String>,
    },
}

impl Error {
    pub(crate) fn fatal(message: impl Into<String>) -> Self {
        Self::Fatal {
            message: message.into(),
            hint: None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::Interrupted => Self::Interrupted,
            _ => Self::fatal(e.to_string()),
        }
    }
}

impl From<git::Error> for Error {
    fn from(e: git::Error) -> Self {
        let hint = match &e {
            git::Error::NotARepository { .. } => Some(
                "run dffy inside a repository or point it at one with -C <path>; \
                 a diff piped on stdin works anywhere"
                    .to_owned(),
            ),
            git::Error::UnknownRevision {
                rev, suggestion, ..
            } => match suggestion {
                Some(s) => Some(format!("did you mean '{s}'?")),
                None if Path::new(rev).exists() => Some(format!(
                    "'{rev}' is a path; put paths after --, as in: dffy -- {rev}"
                )),
                None => {
                    let command = Cli::command();
                    let names = command
                        .get_subcommands()
                        .filter(|s| !s.is_hide_set())
                        .map(clap::Command::get_name);
                    dffy_core::suggest::closest(rev, names)
                        .map(|name| format!("did you mean the subcommand '{name}'?"))
                }
            },
            git::Error::NoWorktree => {
                Some("compare revisions instead, as in: dffy HEAD~1 HEAD".to_owned())
            }
            _ => None,
        };
        Self::Fatal {
            message: e.to_string(),
            hint,
        }
    }
}

/// Settings and terminal facts every command reads.
#[derive(Clone)]
pub(crate) struct Ctx {
    /// Flags over the config file over defaults; the viewer's settings
    /// come from [`Ctx::viewer`].
    pub config: Config,
    pub sources: config::Sources,
    /// The view choices the viewer remembered; empty with `remember = false`.
    pub last_used: Vec<(String, String)>,
    /// The viewer's first-run tip was shown on an earlier run.
    pub tip_shown: bool,
    /// Lines of context the viewer shows when files aren't whole: `context`,
    /// or under a remembered `full`, the value from before it, so a
    /// configured number survives.
    pub context_lines: usize,
    pub config_path: Option<PathBuf>,
    pub config_warnings: Vec<Warning>,
    pub theme: Theme,
    pub term: Term,
    pub color_out: bool,
    pub color_err: bool,
    /// `None` when the cache is off or there's nowhere to put it.
    pub cache: Option<Cache>,
    /// `--comments`, resolved; the viewer defaults to the repository's.
    pub comments: Option<PathBuf>,
}

impl Ctx {
    #[must_use]
    pub(crate) fn highlight(&self) -> bool {
        self.color_out && self.config.highlight
    }

    #[must_use]
    pub(crate) fn out_theme(&self) -> Option<&Theme> {
        self.color_out.then_some(&self.theme)
    }

    fn err_theme(&self) -> Option<&Theme> {
        self.color_err.then_some(&self.theme)
    }

    /// The diff layout for this terminal. Off a terminal, unified lines are
    /// written in full so pipes lose nothing.
    #[must_use]
    pub(crate) fn layout(&self) -> Layout {
        let width = self.term.width.unwrap_or(80);
        let split = match self.config.view {
            View::Split => true,
            View::Unified => false,
            View::Auto => self.term.stdout_tty && width >= self.config.split_min_width,
        };
        Layout {
            split,
            width: (self.term.stdout_tty || split || self.config.wrap).then_some(width),
            wrap: self.config.wrap,
            words: false,
            tab_width: self.config.tab_width,
            indicators: self.indicators(),
            one_sided: false,
            unmerged: false,
            digits: 0,
            inline_signs: !self.color_out,
            scroll: 0,
            line_numbers: self.config.line_numbers,
            hunk_headers: self.config.hunk_headers,
            modified_lines: self.config.modified_lines == ModifiedLines::Changed,
        }
    }

    /// The configured change indicators, or classic signs when stdout isn't
    /// colored: without color, tints, bars and color-only marks vanish.
    #[must_use]
    pub(crate) fn indicators(&self) -> Indicators {
        indicators(self.config.indicators, self.color_out)
    }

    /// These settings with the viewer's remembered choices laid between the
    /// config file and this run's flags. Only the viewer takes them: print,
    /// pager and `--color-only` output stay what the config says, so scripts
    /// see the same bytes.
    #[must_use]
    pub(crate) fn viewer(&self) -> Self {
        let mut ctx = self.clone();
        ctx.config
            .layer(&mut ctx.sources, &self.last_used, config::Source::LastUsed);
        if let config::Context::Lines(n) = ctx.config.context {
            ctx.context_lines = n;
        }
        if ctx.config != self.config {
            ctx.theme = load_theme(&ctx.config, ctx.color_out, &mut Vec::new());
        }
        ctx
    }
}

/// `config`'s theme, tinted when it marks changes with backgrounds, or the
/// default theme with a warning when it can't be loaded.
fn load_theme(config: &Config, color: bool, warnings: &mut Vec<Warning>) -> Theme {
    let mut theme = Theme::load(&config.theme, warnings).unwrap_or_else(|message| {
        warnings.push(Warning {
            file: None,
            line: 0,
            message,
            hint: Some("dffy themes lists the available themes".to_owned()),
        });
        Theme::default_theme()
    });
    if indicators(config.indicators, color) == Indicators::Background {
        theme.tint(if config.background == config::Background::Light {
            theme::LIGHT_BACKGROUND
        } else {
            theme::DARK_BACKGROUND
        });
    }
    theme
}

/// `configured` resolved: `auto` by whether the terminal takes 24-bit
/// color, since a 16-color green or red background is too harsh to read
/// under text.
fn indicators(configured: Indicators, color: bool) -> Indicators {
    let truecolor = || std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit");
    match configured {
        _ if !color => Indicators::Classic,
        Indicators::Auto if truecolor() => Indicators::Background,
        Indicators::Auto => Indicators::Bars,
        other => other,
    }
}

impl Ctx {
    fn output(&self) -> Output {
        if self.config.paging == Paging::Never || !self.term.stdout_tty {
            return Output::stdout();
        }
        match output::pager_command(&self.config.pager) {
            Some(command) => Output::paged(&command),
            None => Output::stdout(),
        }
    }
}

/// Runs dffy with the process's arguments.
pub fn main() -> ExitCode {
    let mut raw = std::env::args_os();
    if raw.nth(1).is_some_and(|a| a == plugin::review::DETACH) {
        return plugin::review::exec_detached(raw);
    }
    let args = cli::with_default_subcommand(std::env::args_os().collect());
    let mut command = Cli::command();
    if let Some(choice) = color_flag(&args) {
        command = command.color(choice);
    }
    let cli = match command
        .try_get_matches_from(&args)
        .and_then(|m| Cli::from_arg_matches(&m))
    {
        Ok(cli) => cli,
        Err(e) => {
            e.print().ok();
            return if e.use_stderr() {
                ExitCode::from(FATAL)
            } else {
                ExitCode::SUCCESS
            };
        }
    };

    let loaded = config::load();
    let mut config = loaded.config;
    let mut sources = loaded.sources;
    let mut warnings = loaded.warnings;
    let remembered = state::read();
    let last_used: Vec<(String, String)> = remembered
        .iter()
        .filter(|(k, _)| config.remember && state::REMEMBERED.contains(&k.as_str()))
        .cloned()
        .collect();
    for key in apply_flags(&mut config, &cli) {
        sources.set(key, config::Source::Flag);
    }
    let tip_shown = remembered.iter().any(|(k, _)| k == state::TIP_SHOWN);
    let term = Term::detect();
    // git runs the filter with a pipe for stdout, and colors are its point.
    let color_only = matches!(&cli.command, Some(Command::Diff(d)) if d.color_only);
    let color_out = output::use_color(config.color, term.stdout_tty || color_only);
    let theme = load_theme(&config, color_out, &mut warnings);
    let cache = config
        .cache
        .then(Cache::default_dir)
        .flatten()
        .map(|dir| Cache::new(dir, config.cache_size));
    let ctx = Ctx {
        color_out,
        color_err: output::use_color(config.color, term.stderr_tty),
        cache,
        context_lines: config.context.hunk_lines(true),
        config,
        sources,
        last_used,
        tip_shown,
        config_path: loaded.path,
        config_warnings: warnings,
        theme,
        term,
        comments: cli
            .global
            .comments
            .as_deref()
            .and_then(|file| comments_file(cli.global.dir.as_deref(), Some(file)).ok()),
    };
    let mut canvas = Canvas::default();
    for warning in &ctx.config_warnings {
        warning.render(&mut canvas);
    }
    print_err(&ctx, &canvas);

    match run(cli, &ctx) {
        Ok(code) => code,
        Err(Error::BrokenPipe) => ExitCode::SUCCESS,
        Err(Error::Interrupted) => ExitCode::from(INTERRUPTED),
        Err(Error::Fatal { message, hint }) => {
            let mut c = Canvas::default();
            c.push(Role::Error, "error:");
            c.push(Role::Plain, " ");
            c.line(Role::Plain, &render::escape_lines(&message));
            if let Some(hint) = hint {
                c.push(Role::Hint, "hint:");
                c.push(Role::Plain, " ");
                c.line(Role::Plain, &render::escape_lines(&hint));
            }
            print_err(&ctx, &c);
            ExitCode::from(FATAL)
        }
    }
}

/// `--color` decides clap's own colors too, so it is read before parsing.
fn color_flag(args: &[std::ffi::OsString]) -> Option<ColorChoice> {
    let mut value = None;
    let mut iter = args.iter().skip(1).filter_map(|a| a.to_str());
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if let Some(v) = arg.strip_prefix("--color=") {
            value = Some(v.to_owned());
        } else if arg == "--color" {
            value = iter.next().map(str::to_owned);
        }
    }
    match value.as_deref() {
        Some("always") => Some(ColorChoice::Always),
        Some("never") => Some(ColorChoice::Never),
        _ => None,
    }
}

/// Flags win over the config file and what the viewer remembered. Returns
/// the config keys they set.
fn apply_flags(config: &mut Config, cli: &Cli) -> Vec<&'static str> {
    let g = &cli.global;
    let mut set = Vec::new();
    if let Some(color) = g.color {
        config.color = color;
        set.push("color");
    }
    if let Some(paging) = g.paging {
        config.paging = paging;
        set.push("paging");
    }
    if let Some(theme) = &g.theme {
        config.theme.clone_from(theme);
        set.push("theme");
    }
    if g.no_cache {
        config.cache = false;
        set.push("cache");
    }
    if let Some(review) = &g.review {
        config.review = Some(review.clone());
        set.push("review");
    }
    let (view, watch) = match &cli.command {
        Some(Command::Diff(d)) => (&d.common.view, d.watch),
        Some(Command::Show(s)) => (&s.common.view, false),
        _ => return set,
    };
    apply_view(config, view, &mut set);
    if watch {
        config.watch = true;
        set.push("watch");
    }
    set
}

fn apply_view(config: &mut Config, v: &ViewArgs, set: &mut Vec<&'static str>) {
    let mut flag = |key, on: bool| {
        if on {
            set.push(key);
        }
        on
    };
    if let Some(context) = v.context() {
        config.context = context;
        flag("context", true);
    }
    if flag("view", v.split) {
        config.view = View::Split;
    } else if flag("view", v.unified_view()) {
        config.view = View::Unified;
    }
    // Either flag decides both keys: a remembered value of the other one
    // would otherwise override `-b` with `-w`, or the reverse.
    let whitespace = v.ignore_all_space || v.ignore_space_change;
    flag("ignore-all-space", whitespace);
    if flag("ignore-space-change", whitespace) {
        config.whitespace = if v.ignore_all_space {
            dffy_core::diff::Whitespace::IgnoreAll
        } else {
            dffy_core::diff::Whitespace::IgnoreChange
        };
    }
    if flag("renames", v.no_renames) {
        config.renames = false;
    }
    if flag("wrap", v.wrap) {
        config.wrap = true;
    } else if flag("wrap", v.no_wrap) {
        config.wrap = false;
    }
    if flag("highlight", v.highlight) {
        config.highlight = true;
    } else if flag("highlight", v.no_highlight) {
        config.highlight = false;
    }
    if flag("syntax-diff", v.syntax) {
        config.syntax_diff = true;
    } else if flag("syntax-diff", v.no_syntax) {
        config.syntax_diff = false;
    }
}

fn print_err(ctx: &Ctx, canvas: &Canvas) {
    if canvas.is_empty() {
        return;
    }
    let mut buf = Vec::new();
    canvas.write_to(&mut buf, ctx.err_theme()).ok();
    io::stderr().lock().write_all(&buf).ok();
}

/// `dffy doctor`: fails when a check does.
fn doctor(ctx: &Ctx, dir: Option<&Path>, json: bool) -> Result<ExitCode, Error> {
    let facts = doctor::Facts::gather(ctx, dir);
    let failed = if json {
        let (text, failed) = facts.json();
        io::stdout().lock().write_all(text.as_bytes())?;
        failed
    } else {
        let mut c = Canvas::default();
        let failed = facts.render(&mut c);
        // Its exit code is always success; the checks decide doctor's.
        let _ = write_stdout(ctx, &c)?;
        failed
    };
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The comments file: `file` (`--comments`) relative to where dffy runs
/// (`-C`), or the default in the repository's git directory.
fn comments_file(dir: Option<&Path>, file: Option<&Path>) -> Result<PathBuf, Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| Error::fatal(format!("can't read the current directory: {e}")))?;
    let base = dir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir));
    if let Some(file) = file {
        return Ok(base.join(file));
    }
    let not_found = || Error::Fatal {
        message: format!(
            "{} isn't in a git repository, so it has no comments file",
            base.display()
        ),
        hint: Some(
            "name one with --comments <file>, or run dffy inside a repository or with -C <path>"
                .to_owned(),
        ),
    };
    let repo = git::Repo::discover(&base).map_err(|_| not_found())?;
    let git_dir = repo.git_dir().ok_or_else(not_found)?;
    Ok(review::default_path(git_dir))
}

/// `dffy comments`: the file listed, printed as JSON, or written by `apply`.
fn comments(
    path: &Path,
    json: bool,
    action: Option<CommentsAction>,
    ctx: &Ctx,
) -> Result<ExitCode, Error> {
    match action {
        Some(CommentsAction::Apply) => {
            use std::io::{IsTerminal, Read};
            if io::stdin().is_terminal() {
                return Err(Error::Fatal {
                    message:
                        "dffy comments apply reads operations from stdin, and stdin is a terminal"
                            .into(),
                    hint: Some(
                        "pipe a JSON list in, as in: echo '[…]' | dffy comments apply".into(),
                    ),
                });
            }
            let mut input = Vec::new();
            io::stdin().lock().read_to_end(&mut input)?;
            let answer = review::apply_json(path, &input).map_err(Error::fatal)?;
            io::stdout().lock().write_all(answer.as_bytes())?;
            Ok(ExitCode::SUCCESS)
        }
        None => {
            let file = review::read(path).map_err(Error::fatal)?;
            if json {
                io::stdout().lock().write_all(file.encode().as_bytes())?;
                return Ok(ExitCode::SUCCESS);
            }
            let mut c = Canvas::default();
            if let Some(notice) = &file.notice {
                c.push(Role::Warn, "notice:");
                c.line(
                    Role::Plain,
                    &format!(" {}", render::escape(notice.as_bytes())),
                );
                c.end_line();
            }
            if file.comments.is_empty() {
                c.line(Role::Dim, "no comments");
            }
            review::render(&review::threads(&file.comments), &mut c);
            write_stdout(ctx, &c)
        }
    }
}

fn run(cli: Cli, ctx: &Ctx) -> Result<ExitCode, Error> {
    let dir = cli.global.dir.as_deref();
    match cli.command {
        None => diff(dir, &DiffArgs::default(), ctx),
        Some(Command::Diff(args)) => diff(dir, &args, ctx),
        Some(Command::Show(args)) => show(dir, &args, ctx),
        Some(Command::Comments { json, action }) => {
            let path = comments_file(dir, cli.global.comments.as_deref())?;
            comments(&path, json, action, ctx)
        }
        Some(Command::Config {
            markdown: Some(page),
            ..
        }) => {
            let text = match page {
                cli::DocPage::Keys => docs::keys(),
                cli::DocPage::Config => docs::config(),
            };
            io::stdout().lock().write_all(text.as_bytes())?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Config { default, .. }) => {
            let mut c = Canvas::default();
            if default {
                config::render_defaults(&mut c);
            } else {
                let source = match &ctx.config_path {
                    Some(path) => format!("# config file: {}", path.display()),
                    None => match config::config_dir() {
                        Some(dir) => format!(
                            "# no config file at {}; showing defaults",
                            dir.join("config").display()
                        ),
                        None => "# no config directory; showing defaults".to_owned(),
                    },
                };
                c.line(Role::Dim, &source);
                // What the viewer runs with, remembered choices included,
                // each marked with where it comes from.
                let viewer = ctx.viewer();
                viewer.config.render(&viewer.sources, &mut c);
            }
            write_stdout(ctx, &c)
        }
        Some(Command::Themes { dump: Some(name) }) => {
            let text = theme::dump(&name).map_err(|message| Error::Fatal {
                message,
                hint: Some("dffy themes lists the built-in themes".to_owned()),
            })?;
            let mut c = Canvas::default();
            for line in text.lines() {
                c.line(Role::Plain, line);
            }
            write_stdout(ctx, &c)
        }
        Some(Command::Themes { dump: None }) => {
            let mut out = Output::stdout();
            theme::list(ctx, &mut out)?;
            out.finish()?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Doctor { json }) => doctor(ctx, dir, json),
        Some(Command::Pr { number }) => pr(dir, number, ctx),
        Some(Command::Daemon { action }) => {
            let mut c = Canvas::default();
            match action {
                DaemonAction::Status => daemon::status(&mut c)?,
                DaemonAction::Stop => daemon::stop(&mut c)?,
                DaemonAction::Run => {
                    daemon::broker::run()?;
                    return Ok(ExitCode::SUCCESS);
                }
                DaemonAction::Watch { path } => {
                    let path = match dir {
                        Some(dir) => dir.join(path),
                        None => path,
                    };
                    daemon::watch(&path)?;
                    return Ok(ExitCode::SUCCESS);
                }
            }
            write_stdout(ctx, &c)
        }
        Some(Command::Mcp) => {
            mcp::serve(dir, cli.global.comments.as_deref(), ctx)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Completions { shell }) => {
            let mut buf = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "dffy", &mut buf);
            io::stdout().lock().write_all(&buf)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Man) => {
            let mut buf = Vec::new();
            clap_mangen::Man::new(Cli::command()).render(&mut buf)?;
            io::stdout().lock().write_all(&buf)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn write_stdout(ctx: &Ctx, c: &Canvas) -> Result<ExitCode, Error> {
    let mut out = Output::stdout();
    out.write(c, ctx.out_theme())?;
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}

fn diff(dir: Option<&Path>, args: &DiffArgs, ctx: &Ctx) -> Result<ExitCode, Error> {
    if args.color_only {
        return color_only(args, ctx);
    }
    let cwd = std::env::current_dir()
        .map_err(|e| Error::fatal(format!("can't read the current directory: {e}")))?;
    let base = dir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir));
    if let Some(operands) = no_index::operands(&base, args)? {
        return compare(&base, &operands, args, ctx);
    }
    if !args.exclude.is_empty() || args.gitignore {
        return Err(Error::Fatal {
            message: "--exclude and --gitignore apply only when comparing folders".into(),
            hint: Some("as in: dffy --exclude target/ dirA dirB".into()),
        });
    }
    let reads_stdin = match args.revs.as_slice() {
        [dash] if dash == "-" && args.json => {
            return Err(Error::Fatal {
                message: "--json reads the repository; it can't read a diff from stdin".into(),
                hint: None,
            });
        }
        [dash] if dash == "-" => true,
        revs if revs.iter().any(|r| r == "-") => {
            return Err(Error::Fatal {
                message: "'-' (read a diff from stdin) can't be combined with revisions".into(),
                hint: None,
            });
        }
        [] => !selects_from_repo(args) && stdin_has_diff(),
        _ => false,
    };
    if reads_stdin {
        if ctx.term.stdout_tty {
            return piped(ctx);
        }
        return print_diff(io::stdin().lock(), ctx.output(), ctx);
    }
    let selected = args.staged || args.merge_base || !args.revs.is_empty();
    let revisions = git::Revisions {
        revs: args.revs.clone(),
        staged: args.staged,
        unstaged: args.unstaged
            || (!selected && !args.all && ctx.config.default_diff == DefaultDiff::Unstaged),
        merge_base: args.merge_base,
    };
    let target = print::Target::Diff(&revisions);
    let c = &args.common;
    if args.json {
        let paths = printed_paths(c);
        let plan = print::prepare(dir, &target, paths, print::Summary::Diff, c.view.reverse)?;
        let compared = json::Compared {
            revisions: &args.revs,
            operands: None,
        };
        return json_diff(&plan, &compared, args, ctx);
    }
    let viewer = ctx.term.stdout_tty
        && ctx.config.paging != Paging::Never
        && !(args.exit_code || c.stat || c.name_only || c.name_status);
    if viewer && let Ok(tty) = term::Tty::open() {
        let plan = print::prepare(dir, &target, &c.paths, print::Summary::Diff, c.view.reverse)?;
        let focus = c.focus.as_deref().map(|f| plan.relative(f)).transpose()?;
        let description = describe(&revisions, c.view.reverse);
        let detect = detect_pr(args, &plan, ctx);
        return tui::run(
            &tty,
            plan,
            revisions,
            focus,
            description,
            detect,
            &ctx.viewer(),
        );
    }
    if ctx.config.watch {
        warn(ctx, "--watch needs the viewer on a terminal; printing once");
    }
    run_print(dir, &target, &args.common, args.exit_code, ctx)
}

/// `dffy pr`: the viewer on PR `number`, or the current branch's, or the
/// picker of PRs on the diff `dffy` shows. It opens once the plugin's
/// `prepare` names the PR's commits, and the plugin goes on downloading
/// threads behind it.
fn pr(dir: Option<&Path>, number: Option<u64>, ctx: &Ctx) -> Result<ExitCode, Error> {
    // The plugin runs in a session of its own, which the terminal's Ctrl-C
    // doesn't reach: until the viewer handles signals, they kill it here.
    let phase = Arc::new(Mutex::new(Phase::Waiting));
    let handler = Arc::clone(&phase);
    term::on_signals(move |signal| {
        let mut phase = handler.lock().unwrap_or_else(PoisonError::into_inner);
        if signal == term::Signal::Interrupt && *phase == Phase::Waiting {
            *phase = Phase::Interrupted;
            plugin::stop_all();
        }
        *phase == Phase::Waiting
    })?;
    let result = open_pr(dir, number, ctx, &phase);
    // A `prepare` started before an error is stopped too, not only one the
    // viewer ran.
    plugin::stop_all();
    if *phase.lock().unwrap_or_else(PoisonError::into_inner) == Phase::Interrupted {
        return Err(Error::Interrupted);
    }
    result
}

/// How far `dffy pr` got, for a signal before its viewer handles them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Interrupted,
    Viewing,
}

fn open_pr(
    dir: Option<&Path>,
    number: Option<u64>,
    ctx: &Ctx,
    phase: &Mutex<Phase>,
) -> Result<ExitCode, Error> {
    use plugin::review::{self, Reason, Said};
    let (name, program) = review_plugin(ctx)?;
    let tty = ctx
        .term
        .stdout_tty
        .then(term::Tty::open)
        .and_then(Result::ok)
        .ok_or_else(|| Error::fatal("dffy pr opens the viewer, which needs a terminal"))?;
    let cwd = std::env::current_dir()
        .map_err(|e| Error::fatal(format!("can't read the current directory: {e}")))?;
    let repo = git::Repo::discover(&dir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir)))?;
    let root = repo.workdir().ok_or(git::Error::NoWorktree)?;
    let program_name = format!("{}{name}", plugin::REVIEW_PREFIX);
    let start = |args: &[String]| -> Result<_, Error> {
        let (tx, rx) = std::sync::mpsc::channel();
        let command = review::command(&program.path, args, root, None)?;
        let run = review::start(command, &program_name, &args[0], move |said| {
            tx.send(said).is_ok()
        })
        .map_err(|e| Error::fatal(format!("couldn't start {program_name}: {e}")))?;
        Ok((run, rx))
    };
    // Until the viewer takes the screen.
    let say = |text: &str| {
        if ctx.term.stderr_tty {
            eprint!("\r\x1b[K{text}");
        }
    };
    let viewer = |revisions: git::Revisions, description, start, viewer: &Ctx| {
        {
            let mut phase = phase.lock().unwrap_or_else(PoisonError::into_inner);
            if *phase != Phase::Waiting {
                return Err(Error::Interrupted);
            }
            *phase = Phase::Viewing;
        }
        let target = print::Target::Diff(&revisions);
        let plan = print::prepare(dir, &target, &[], print::Summary::Diff, false)?;
        tui::run(
            &tty,
            plan,
            revisions,
            None,
            description,
            Some(start),
            viewer,
        )
    };
    let number = if let Some(number) = number {
        number
    } else {
        say("listing pull requests…");
        let (_, rx) = start(&["list".to_owned()])?;
        let prs = first_said(&rx, |said| match said {
            Said::Prs(prs) => Some(prs),
            _ => None,
        });
        say("");
        let prs = prs.map_err(|e| {
            Error::fatal(e.unwrap_or_else(|| format!("{program_name} listed nothing")))
        })?;
        if let Some(pr) = prs.iter().find(|p| p.reason == Reason::CurrentBranch) {
            pr.number
        } else {
            let revisions = git::Revisions {
                unstaged: ctx.config.default_diff == DefaultDiff::Unstaged,
                ..git::Revisions::default()
            };
            let description = describe(&revisions, false);
            let pick = tui::PrStart::Pick { plugin: name, prs };
            return viewer(revisions, description, pick, &ctx.viewer());
        }
    };
    say(&format!("fetching #{number}…"));
    let (run, rx) = start(&["prepare".to_owned(), number.to_string()])?;
    let session = first_said(&rx, |said| match said {
        Said::Session(session) => Some(session),
        _ => None,
    });
    say("");
    let session = session.map_err(|e| {
        let e = e.unwrap_or_else(|| format!("{program_name} prepare named no commits"));
        Error::fatal(format!("PR #{number}: {e}"))
    })?;
    let revisions = git::Revisions {
        revs: vec![format!("{}...{}", session.base, session.head)],
        ..git::Revisions::default()
    };
    let mut ctx = ctx.viewer();
    ctx.comments = Some(session.comments.clone());
    let open = tui::PrStart::Open {
        plugin: name,
        session,
        run,
        rest: rx,
    };
    viewer(revisions, format!("PR #{number}"), open, &ctx)
}

/// Plain `dffy` (no revisions or paths) on a branch, with a review plugin
/// set: the viewer asks the plugin for the branch's PR as it starts.
fn detect_pr(args: &DiffArgs, plan: &print::Plan, ctx: &Ctx) -> Option<tui::PrStart> {
    let unnamed = args.revs.is_empty()
        && !(args.staged || args.unstaged || args.merge_base || args.common.view.reverse)
        && args.common.paths.is_empty();
    let on_branch = || {
        plan.repo
            .git_dir()
            .and_then(|dir| std::fs::read(dir.join("HEAD")).ok())
            .is_some_and(|head| head.starts_with(b"ref: refs/heads/"))
    };
    let plugin = ctx
        .config
        .review
        .clone()
        .filter(|_| unnamed && on_branch())?;
    Some(tui::PrStart::Detect { plugin })
}

/// The review plugin set, and its program.
fn review_plugin(ctx: &Ctx) -> Result<(String, plugin::Program), Error> {
    let Some(name) = ctx.config.review.clone() else {
        return Err(Error::Fatal {
            message: "dffy pr needs a review plugin, and none is set".into(),
            hint: Some(
                "set review = <name> in the config, or pass --review <name>; see docs/plugins.md"
                    .into(),
            ),
        });
    };
    let program = plugin::find(plugin::REVIEW_PREFIX, &name).ok_or_else(|| Error::Fatal {
        message: format!("{}{name} isn't on PATH", plugin::REVIEW_PREFIX),
        hint: Some("docs/install.md says how to install it".into()),
    })?;
    Ok((name, program))
}

/// The first line on `said` that `pick` takes; `Err` with the run's
/// error when it ended first, `Err(None)` when it ended well.
fn first_said<T>(
    said: &std::sync::mpsc::Receiver<plugin::review::Said>,
    mut pick: impl FnMut(plugin::review::Said) -> Option<T>,
) -> Result<T, Option<String>> {
    use plugin::review::Said;
    loop {
        match said.recv() {
            Ok(Said::Done(result)) => return Err(result.err()),
            Ok(line) => {
                if let Some(picked) = pick(line) {
                    return Ok(picked);
                }
            }
            Err(_) => return Err(None),
        }
    }
}

/// `dffy A B`: two files or folders outside git (see [`no_index`]).
fn compare(
    base: &Path,
    operands: &[PathBuf; 2],
    args: &DiffArgs,
    ctx: &Ctx,
) -> Result<ExitCode, Error> {
    let c = &args.common;
    let difftool = no_index::Difftool::from_env();
    let (repo, description) = no_index::open(base, operands, args, &difftool)?;
    if args.json {
        let mut plan = print::Plan::compare(repo, base, print::Summary::Diff, c.view.reverse);
        if let Some(focus) = &c.focus {
            plan.paths = vec![plan.relative(focus)?];
        }
        let compared = json::Compared {
            revisions: &[],
            operands: Some(operands),
        };
        return json_diff(&plan, &compared, args, ctx);
    }
    let viewer = ctx.term.stdout_tty
        && ctx.config.paging != Paging::Never
        && !(args.exit_code || c.stat || c.name_only || c.name_status);
    if viewer && let Ok(tty) = term::Tty::open() {
        let plan = print::Plan::compare(repo, base, print::Summary::Diff, c.view.reverse);
        let focus = c.focus.as_deref().map(|f| plan.relative(f)).transpose()?;
        let revisions = git::Revisions::default();
        return tui::run(
            &tty,
            plan,
            revisions,
            focus,
            description,
            None,
            &ctx.viewer(),
        );
    }
    if ctx.config.watch {
        warn(ctx, "--watch needs the viewer on a terminal; printing once");
    }
    let mut plan = print::Plan::compare(repo, base, summary(c), c.view.reverse);
    if let Some(focus) = &c.focus {
        plan.paths = vec![plan.relative(focus)?];
    }
    print_plan(&plan, args.exit_code, ctx)
}

/// `--color-only`: stdin colored to stdout, line for line. Never a pager
/// or the terminal, and never a wait on one.
fn color_only(args: &DiffArgs, ctx: &Ctx) -> Result<ExitCode, Error> {
    use std::io::IsTerminal;
    if args.revs.iter().any(|r| r != "-") {
        return Err(Error::fatal(
            "--color-only colors a diff read from stdin; it takes no revisions",
        ));
    }
    if io::stdin().is_terminal() {
        return Err(Error::Fatal {
            message: "--color-only reads a diff from stdin, and stdin is a terminal".into(),
            hint: Some(
                "pipe a diff in, or set it as git's filter: \
                 git config interactive.diffFilter \"dffy --color-only\""
                    .into(),
            ),
        });
    }
    pager::color_only::run(
        io::stdin().lock(),
        io::BufWriter::with_capacity(64 << 10, io::stdout().lock()),
        ctx.out_theme(),
        ctx.indicators() == Indicators::Background,
    )?;
    Ok(ExitCode::SUCCESS)
}

fn print_diff(input: impl io::Read, mut out: Output, ctx: &Ctx) -> Result<ExitCode, Error> {
    let theme = ctx.out_theme();
    let result = pager::run(input, ctx.layout(), |sink| match sink {
        pager::Sink::Write(canvas) => out.write(canvas, theme),
        pager::Sink::Flush => out.flush(),
    });
    let finished = out.finish();
    result?;
    finished?;
    Ok(ExitCode::SUCCESS)
}

/// Piped input with a terminal for output. A diff opens the viewer, unless
/// it fits one screen (printed, as `less -F` would) or paging is off
/// (printed); anything else passes through to the pager unchanged, so
/// `git log`, `git branch` and `git blame` look as they would without dffy.
fn piped(ctx: &Ctx) -> Result<ExitCode, Error> {
    use pager::stdin::{Decision, Stdin, decide};
    let mut stdin = Stdin::spawn()?;
    let tty = if ctx.config.paging == Paging::Never {
        None
    } else {
        term::Tty::open().ok()
    };
    let layout = ctx.layout();
    let height = tty.as_ref().map(|t| t.size().1);
    match decide(&mut stdin, height.map(|h| (h, layout.split)))? {
        Decision::NotDiff => {
            let mut out = ctx.output();
            let mut buf = vec![0; 64 << 10];
            let copied = loop {
                match io::Read::read(&mut stdin, &mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        if let Err(e) = out.write_raw(&buf[..n]).and_then(|()| out.flush()) {
                            break Err(e);
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            let finished = out.finish();
            copied?;
            finished?;
            Ok(ExitCode::SUCCESS)
        }
        Decision::Print => print_diff(stdin, ctx.output(), ctx),
        Decision::Fits => {
            // The decision went by a lower bound; the rendering itself
            // says whether it fits.
            let mut rows = 0;
            let mut bytes = Vec::new();
            let theme = ctx.out_theme();
            pager::run(stdin.buffered(), layout, |sink| {
                if let pager::Sink::Write(canvas) = sink {
                    rows += canvas.rows().count();
                    canvas.write_to(&mut bytes, theme)?;
                }
                Ok(())
            })?;
            match tty {
                Some(tty) if height.is_some_and(|h| rows > h) => {
                    tui::run_piped(&tty, stdin, &ctx.viewer())
                }
                _ => {
                    io::stdout().lock().write_all(&bytes)?;
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
        Decision::Viewer => match tty {
            Some(tty) => tui::run_piped(&tty, stdin, &ctx.viewer()),
            None => print_diff(stdin, ctx.output(), ctx),
        },
    }
}

/// What a diff compares, as a short phrase.
fn describe(r: &git::Revisions, reverse: bool) -> String {
    let mut s = match (r.revs.is_empty(), r.staged) {
        (true, false) if r.unstaged => "unstaged".to_owned(),
        (true, false) => "uncommitted".to_owned(),
        (true, true) => "staged".to_owned(),
        (false, staged) => {
            let revs = r.revs.join(" ");
            if staged {
                format!("{revs} staged")
            } else {
                revs
            }
        }
    };
    if r.merge_base {
        s.push_str(" from merge base");
    }
    if reverse {
        s.push_str(" reversed");
    }
    s
}

fn show(dir: Option<&Path>, args: &ShowArgs, ctx: &Ctx) -> Result<ExitCode, Error> {
    let c = &args.common;
    let viewer = ctx.term.stdout_tty
        && ctx.config.paging != Paging::Never
        && !(c.stat || c.name_only || c.name_status);
    if viewer && let Ok(tty) = term::Tty::open() {
        let target = print::Target::Show(&args.rev);
        let plan = print::prepare(dir, &target, &c.paths, print::Summary::Diff, c.view.reverse)?;
        let focus = c.focus.as_deref().map(|f| plan.relative(f)).transpose()?;
        let id = plan.commit.as_ref().map(|info| info.id.to_string());
        let revisions = git::Revisions {
            // The commit's own id, so a rescan finds the same trees.
            revs: vec![format!("{}^!", id.as_deref().unwrap_or(&args.rev))],
            ..git::Revisions::default()
        };
        let description = format!("show {}", args.rev);
        return tui::run(
            &tty,
            plan,
            revisions,
            focus,
            description,
            None,
            &ctx.viewer(),
        );
    }
    run_print(
        dir,
        &print::Target::Show(&args.rev),
        &args.common,
        false,
        ctx,
    )
}

fn run_print(
    dir: Option<&Path>,
    target: &print::Target,
    common: &CommonArgs,
    exit_code: bool,
    ctx: &Ctx,
) -> Result<ExitCode, Error> {
    let paths = printed_paths(common);
    let plan = print::prepare(dir, target, paths, summary(common), common.view.reverse)?;
    print_plan(&plan, exit_code, ctx)
}

/// The paths printed output is limited to. In print mode --focus narrows
/// the output to one path; the viewer uses it to pick the starting file
/// instead.
fn printed_paths(common: &CommonArgs) -> &[PathBuf] {
    match &common.focus {
        Some(focus) => std::slice::from_ref(focus),
        None => &common.paths,
    }
}

/// `--json`: straight to stdout, never paged.
fn json_diff(
    plan: &print::Plan,
    compared: &json::Compared,
    args: &DiffArgs,
    ctx: &Ctx,
) -> Result<ExitCode, Error> {
    let detail = if summary(&args.common) != print::Summary::Diff {
        json::Detail::Summary
    } else if args.highlight_spans {
        json::Detail::Highlights
    } else {
        json::Detail::Hunks
    };
    let mut out = Output::stdout();
    let result = json::write(plan, compared, detail, ctx, &mut out);
    let finished = out.finish();
    let changed = result?;
    finished?;
    Ok(if args.exit_code && changed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn summary(common: &CommonArgs) -> print::Summary {
    if common.stat {
        print::Summary::Stat
    } else if common.name_only {
        print::Summary::NameOnly
    } else if common.name_status {
        print::Summary::NameStatus
    } else {
        print::Summary::Diff
    }
}

fn print_plan(plan: &print::Plan, exit_code: bool, ctx: &Ctx) -> Result<ExitCode, Error> {
    let mut out = ctx.output();
    let result = plan.run(ctx, &mut out);
    let finished = out.finish();
    let changed = result?;
    finished?;
    Ok(if exit_code && changed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Arguments that only make sense against the repository, so stdin is
/// never read for them.
fn selects_from_repo(args: &DiffArgs) -> bool {
    let c = &args.common;
    args.staged
        || args.unstaged
        || args.all
        || args.merge_base
        || args.exit_code
        || args.json
        || c.stat
        || c.name_only
        || c.name_status
        || c.focus.is_some()
        || !c.paths.is_empty()
}

/// Whether stdin is a pipe or a file. A terminal, `/dev/null` or a socket
/// isn't: those come from shells, CI runners and agent harnesses that never
/// meant to pipe a diff, and reading them could block forever.
fn stdin_has_diff() -> bool {
    use rustix::fs::FileType;
    rustix::fs::fstat(io::stdin()).is_ok_and(|st| {
        matches!(
            FileType::from_raw_mode(st.st_mode),
            FileType::Fifo | FileType::RegularFile
        )
    })
}

fn warn(ctx: &Ctx, message: &str) {
    let mut c = Canvas::default();
    c.push(Role::Warn, "warning:");
    c.push(Role::Plain, " ");
    c.line(Role::Plain, &render::escape_lines(message));
    print_err(ctx, &c);
}

#[cfg(test)]
mod tests {
    use super::*;
    use dffy_core::diff::Whitespace;

    #[test]
    fn a_whitespace_flag_beats_the_other_remembered_whitespace_key() {
        let args = cli::with_default_subcommand(vec!["dffy".into(), "-b".into()]);
        let matches = Cli::command().try_get_matches_from(args).expect("parses");
        let cli = Cli::from_arg_matches(&matches).expect("arguments");
        let mut config = Config::default();
        let mut sources = config::Sources::default();
        for key in apply_flags(&mut config, &cli) {
            sources.set(key, config::Source::Flag);
        }
        let last_used = [("ignore-all-space".to_owned(), "true".to_owned())];
        config.layer(&mut sources, &last_used, config::Source::LastUsed);
        assert_eq!(config.whitespace, Whitespace::IgnoreChange);
    }

    /// `--review` wins over the config's `review` for the run, before the
    /// subcommand too, and nothing remembered replaces it.
    #[test]
    fn the_review_flag_beats_the_config() {
        for args in [
            &["dffy", "--review", "flag", "HEAD"][..],
            &["dffy", "--review=flag", "doctor"],
        ] {
            let args = cli::with_default_subcommand(
                args.iter().map(|a| std::ffi::OsString::from(*a)).collect(),
            );
            let matches = Cli::command().try_get_matches_from(args).expect("parses");
            let cli = Cli::from_arg_matches(&matches).expect("arguments");
            let mut config = Config::default();
            let mut sources = config::Sources::default();
            for key in config.apply_source("review = configured\n", &mut Vec::new()) {
                sources.set(key, config::Source::Config);
            }
            assert_eq!(config.review.as_deref(), Some("configured"));
            for key in apply_flags(&mut config, &cli) {
                sources.set(key, config::Source::Flag);
            }
            let last_used = [("review".to_owned(), "remembered".to_owned())];
            config.layer(&mut sources, &last_used, config::Source::LastUsed);
            assert_eq!(config.review.as_deref(), Some("flag"));
            assert_eq!(sources.get("review"), config::Source::Flag);
        }
        let mut config = Config::default();
        assert_eq!(config.review, None);
        assert!(config.set("review", "Not/A-Name").is_err());
    }
}
