//! The `dffy` binary. It's a library only so benchmarks can reach the
//! renderer; [`main`] is the whole program.

mod cli;
mod config;
mod daemon;
mod docs;
mod doctor;
mod keys;
mod no_index;
mod output;
mod pager;
mod plugin;
mod print;
mod render;
mod state;
mod term;
mod theme;
mod tui;

#[doc(hidden)]
pub mod bench;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ColorChoice, CommandFactory, FromArgMatches};
use dffy_core::cache::Cache;
use dffy_core::git;

use cli::{Cli, Command, CommonArgs, DaemonAction, DiffArgs, ShowArgs, ViewArgs};
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

#[derive(Debug)]
pub(crate) enum Error {
    /// The reader of our output went away; exit quietly with success.
    BrokenPipe,
    /// Ctrl-C while a pager was showing our output; exit quietly.
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
#[must_use]
pub fn main() -> ExitCode {
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
        write_stdout(ctx, &c)?;
        failed
    };
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn run(cli: Cli, ctx: &Ctx) -> Result<ExitCode, Error> {
    let dir = cli.global.dir.as_deref();
    match cli.command {
        None => diff(dir, &DiffArgs::default(), ctx),
        Some(Command::Diff(args)) => diff(dir, &args, ctx),
        Some(Command::Show(args)) => show(dir, &args, ctx),
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
    let viewer = ctx.term.stdout_tty
        && ctx.config.paging != Paging::Never
        && !(args.exit_code || c.stat || c.name_only || c.name_status);
    if viewer && let Ok(tty) = term::Tty::open() {
        let plan = print::prepare(dir, &target, &c.paths, print::Summary::Diff, c.view.reverse)?;
        let focus = c.focus.as_deref().map(|f| plan.relative(f)).transpose()?;
        let description = describe(&revisions, c.view.reverse);
        return tui::run(&tty, plan, revisions, focus, description, &ctx.viewer());
    }
    if ctx.config.watch {
        warn(ctx, "--watch needs the viewer on a terminal; printing once");
    }
    run_print(dir, &target, &args.common, args.exit_code, ctx)
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
    let viewer = ctx.term.stdout_tty
        && ctx.config.paging != Paging::Never
        && !(args.exit_code || c.stat || c.name_only || c.name_status);
    if viewer && let Ok(tty) = term::Tty::open() {
        let plan = print::Plan::compare(repo, base, print::Summary::Diff, c.view.reverse);
        let focus = c.focus.as_deref().map(|f| plan.relative(f)).transpose()?;
        let revisions = git::Revisions::default();
        return tui::run(&tty, plan, revisions, focus, description, &ctx.viewer());
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
        return tui::run(&tty, plan, revisions, focus, description, &ctx.viewer());
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
    // In print mode --focus narrows the output to one path; the viewer uses
    // it to pick the starting file instead.
    let paths: &[PathBuf] = match &common.focus {
        Some(focus) => std::slice::from_ref(focus),
        None => &common.paths,
    };
    let plan = print::prepare(dir, target, paths, summary(common), common.view.reverse)?;
    print_plan(&plan, exit_code, ctx)
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
}
