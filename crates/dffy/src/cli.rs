//! Command-line interface. `diff` is the default subcommand, so
//! `dffy main..HEAD` means `dffy diff main..HEAD`; a revision named like a
//! subcommand needs the explicit form (`dffy diff config`). Diff flags mirror
//! `git diff`.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::config::{ColorWhen, Context, Paging};

#[derive(Debug, Parser)]
#[command(
    name = "dffy",
    version,
    about = "A fast terminal diff viewer",
    long_about = "A fast terminal diff viewer.\n\n\
        Without a subcommand dffy runs `diff` (see `dffy diff --help`). With a \
        diff on stdin (a pipe, a file, or `-`), it renders that diff, so it \
        works as git's pager: `git config core.pager dffy`.",
    styles = styles(),
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Global options that take a value, as they can appear before the
/// subcommand.
const GLOBAL_WITH_VALUE: [&str; 4] = ["-C", "--color", "--paging", "--theme"];

/// Inserts `diff` before the first argument that is neither a global option
/// nor a subcommand, so `dffy -C repo main..HEAD --stat` parses as
/// `dffy -C repo diff main..HEAD --stat`. clap's own way to have arguments
/// without a subcommand turns global options off for the subcommands.
pub(crate) fn with_default_subcommand(mut args: Vec<OsString>) -> Vec<OsString> {
    let command = Cli::command();
    let is_subcommand = |name: &str| {
        name == "help"
            || command
                .get_subcommands()
                .any(|s| s.get_name() == name || s.get_all_aliases().any(|a| a == name))
    };
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].to_str().unwrap_or_default();
        if matches!(arg, "-h" | "--help" | "-V" | "--version") || is_subcommand(arg) {
            return args;
        }
        if GLOBAL_WITH_VALUE.contains(&arg) {
            i += 2;
        } else if GLOBAL_WITH_VALUE.iter().any(|g| {
            arg.strip_prefix(g)
                .is_some_and(|rest| rest.starts_with('=') || (*g == "-C" && !rest.is_empty()))
        }) {
            i += 1;
        } else {
            args.insert(i, OsString::from("diff"));
            return args;
        }
    }
    args
}

#[derive(Debug, Args)]
pub(crate) struct Global {
    /// Run as if dffy was started in this directory
    #[arg(short = 'C', value_name = "path", global = true)]
    pub dir: Option<PathBuf>,
    /// When to use colors; auto follows the terminal and the environment
    #[arg(long, value_name = "when", global = true)]
    pub color: Option<ColorWhen>,
    /// Whether to page output on a terminal; always opens the viewer even
    /// for a piped diff that fits one screen
    #[arg(long, value_name = "when", global = true)]
    pub paging: Option<Paging>,
    /// Theme: a built-in or a file in the config's themes directory
    #[arg(long, value_name = "name", global = true)]
    pub theme: Option<String>,
    /// Neither read nor write the disk cache, for debugging
    #[arg(long, global = true, hide = true)]
    pub no_cache: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Show changes between the working tree, the index and revisions (default)
    Diff(DiffArgs),
    /// Show a commit against its first parent, with its header
    Show(ShowArgs),
    /// Print the effective configuration
    Config {
        /// Print every key with its documentation and default value instead
        #[arg(long)]
        default: bool,
        /// Print a reference page of docs/ in Markdown, from the built-in
        /// defaults; `task docs` writes them
        #[arg(
            long,
            value_enum,
            value_name = "page",
            hide = true,
            conflicts_with = "default"
        )]
        markdown: Option<DocPage>,
    },
    /// List the available themes with a preview
    Themes {
        /// Print a built-in theme as a complete theme file, to copy into
        /// the themes directory and edit
        #[arg(long, value_name = "name")]
        dump: Option<String>,
    },
    /// Check git, the repository, watch limits, the terminal and the config
    Doctor {
        /// Print every check and measurement as one JSON object
        #[arg(long)]
        json: bool,
    },
    /// Inspect or stop the dffyd watch broker
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Serve diffs to coding agents over the Model Context Protocol, on stdin and stdout
    #[command(
        long_about = "Serve diffs to coding agents over the Model Context Protocol.\n\n\
        Speaks MCP on stdin and stdout for the repository at -C or the current \
        directory, with three tools: changed_files, file_diff and diff. They return \
        the objects of dffy --json, a page at a time. Register it with an agent, as \
        in: claude mcp add dffy -- dffy mcp. See docs/mcp.md."
    )]
    Mcp,
    /// Print a shell completion script
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print the man page in roff format
    #[command(hide = true)]
    Man,
}

/// The generated pages of docs/.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum DocPage {
    /// docs/keys.md
    Keys,
    /// The settings table of docs/config.md
    Config,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum DaemonAction {
    /// Show whether dffyd is running and what it watches
    Status,
    /// Stop dffyd; views watching through it carry on by themselves
    Stop,
    /// Run the broker in the foreground (dffy starts it by itself)
    #[command(hide = true)]
    Run,
    /// Subscribe like a view would and print each event, for debugging
    #[command(hide = true)]
    Watch {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Debug, Args, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each git flag is its own switch"
)]
pub(crate) struct DiffArgs {
    /// `<rev>`, `<a> <b>`, `a..b`, `a...b`, two files or folders to compare, or `-` to read a diff from stdin
    #[arg(value_name = "rev")]
    pub revs: Vec<String>,
    /// Compare two files or folders directly, even inside a repository or when they also name revisions
    #[arg(long, conflicts_with_all = ["staged", "unstaged", "all", "merge_base", "color_only"])]
    pub no_index: bool,
    /// Leave paths matching this gitignore-style glob out when comparing folders; repeatable
    #[arg(long, value_name = "glob")]
    pub exclude: Vec<String>,
    /// Leave out what .gitignore files list, and .git, when comparing folders
    #[arg(long)]
    pub gitignore: bool,
    /// Compare the index with HEAD, or with the given revision
    #[arg(long, visible_alias = "cached")]
    pub staged: bool,
    /// Compare the working tree with the index, as plain git diff does: no staged changes, no untracked files
    #[arg(long, conflicts_with_all = ["staged", "merge_base", "revs"])]
    pub unstaged: bool,
    /// Compare HEAD with the working tree: staged, unstaged and untracked changes; the default, unless default-diff = unstaged
    #[arg(
        long,
        conflicts_with_all = ["staged", "unstaged", "merge_base", "revs", "color_only"]
    )]
    pub all: bool,
    /// Compare from the merge base of the revision(s) and HEAD
    #[arg(long)]
    pub merge_base: bool,
    /// Exit with 1 if there are differences, 0 otherwise
    #[arg(long)]
    pub exit_code: bool,
    /// Refresh as files change; `w` toggles it in the viewer
    #[arg(long)]
    pub watch: bool,
    /// Color a diff read from stdin and change nothing else: one output line per input line, for git's interactive.diffFilter
    #[arg(
        long,
        conflicts_with_all = [
            "staged", "unstaged", "merge_base", "exit_code", "watch", "stat", "name_only",
            "name_status",
            "focus", "paths",
        ]
    )]
    pub color_only: bool,
    /// Print the diff as JSON Lines for scripts and agents (docs/json.md): a header, an object per file, an end object; with --stat, --name-only or --name-status, files without hunks. Never colored or paged; named like doctor --json
    #[arg(long, conflicts_with_all = ["color_only", "watch"])]
    pub json: bool,
    /// With --json, add each line's syntax highlighting as role names
    #[arg(long, requires = "json", conflicts_with = "summary")]
    pub highlight_spans: bool,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// The commit to show
    #[arg(value_name = "rev", default_value = "HEAD")]
    pub rev: String,
    #[command(flatten)]
    pub common: CommonArgs,
}

/// Arguments shared by `diff` and `show`.
#[derive(Debug, Args, Default)]
pub(crate) struct CommonArgs {
    /// Only show files under this path
    #[arg(long, value_name = "path")]
    pub focus: Option<PathBuf>,
    /// Show a diffstat instead of the diff
    #[arg(long, group = "summary")]
    pub stat: bool,
    /// Show only the names of changed files
    #[arg(long, group = "summary")]
    pub name_only: bool,
    /// Show the names and status letters of changed files
    #[arg(long, group = "summary")]
    pub name_status: bool,
    #[command(flatten)]
    pub view: ViewArgs,
    /// Limit the diff to these paths, relative to the current directory
    #[arg(last = true, value_name = "path")]
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Args, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each git flag is its own switch"
)]
pub(crate) struct ViewArgs {
    /// Swap the two sides
    #[arg(short = 'R')]
    pub reverse: bool,
    /// Lines of context around each change, or full for the whole file
    #[arg(short = 'U', value_name = "n")]
    pub context: Option<Context>,
    /// Unified layout; with a value (--unified=5 or =full) sets the context lines, as in git
    #[arg(
        long,
        value_name = "n",
        num_args = 0..=1,
        require_equals = true,
        conflicts_with = "split"
    )]
    #[expect(
        clippy::option_option,
        reason = "clap's optional-value form: absent, bare --unified, or --unified=<n>"
    )]
    pub unified: Option<Option<Context>>,
    /// Side-by-side layout
    #[arg(long)]
    pub split: bool,
    /// Ignore whitespace when comparing lines
    #[arg(short = 'w', long)]
    pub ignore_all_space: bool,
    /// Ignore changes in the amount of whitespace
    #[arg(short = 'b', long)]
    pub ignore_space_change: bool,
    /// Don't detect renames; printed output then streams without waiting
    #[arg(long)]
    pub no_renames: bool,
    /// Syntax-highlight code (overrides `highlight = false` in the config)
    #[arg(long, overrides_with = "no_highlight")]
    pub highlight: bool,
    /// Don't syntax-highlight code
    #[arg(long, overrides_with = "highlight")]
    pub no_highlight: bool,
    /// Diff code by syntax: emphasize only changed tokens, dim reformatted code, mark moved code
    #[arg(long, overrides_with = "no_syntax")]
    pub syntax: bool,
    /// Diff by lines (overrides `syntax-diff = true` in the config)
    #[arg(long, overrides_with = "syntax")]
    pub no_syntax: bool,
    /// Wrap long lines instead of truncating them
    #[arg(long, overrides_with = "no_wrap")]
    pub wrap: bool,
    /// Truncate long lines (overrides `wrap = true` in the config)
    #[arg(long, overrides_with = "wrap")]
    pub no_wrap: bool,
}

impl ViewArgs {
    /// The context from `-U<n>` or `--unified=<n>`.
    pub(crate) fn context(&self) -> Option<Context> {
        self.context.or_else(|| self.unified.flatten())
    }

    /// Bare `--unified` asks for the unified layout.
    pub(crate) fn unified_view(&self) -> bool {
        matches!(self.unified, Some(None))
    }
}

/// Help colors on the 16-color palette, matching the default theme.
fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Blue.on_default() | Effects::BOLD)
        .usage(AnsiColor::Blue.on_default() | Effects::BOLD)
        .literal(AnsiColor::Green.on_default())
        .placeholder(AnsiColor::Cyan.on_default())
        .error(AnsiColor::Red.on_default() | Effects::BOLD)
        .valid(AnsiColor::Green.on_default())
        .invalid(AnsiColor::Yellow.on_default())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn parse(args: &[&str]) -> Cli {
        let mut full = vec![OsString::from("dffy")];
        full.extend(args.iter().map(OsString::from));
        Cli::try_parse_from(with_default_subcommand(full))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    fn diff_args(cli: Cli) -> DiffArgs {
        match cli.command {
            Some(Command::Diff(args)) => args,
            other => panic!("expected diff, got {other:?}"),
        }
    }

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_global_option_with_a_value_is_skipped_before_the_subcommand() {
        let command = Cli::command();
        for arg in command
            .get_arguments()
            .filter(|a| a.is_global_set() && a.get_action().takes_values())
        {
            let short = arg.get_short().map(|s| format!("-{s}"));
            let long = arg.get_long().map(|l| format!("--{l}"));
            let name = short.or(long).expect("named option");
            assert!(GLOBAL_WITH_VALUE.contains(&name.as_str()), "{name}");
        }
    }

    #[test]
    fn bare_revisions_mean_diff() {
        assert_eq!(diff_args(parse(&["main..HEAD"])).revs, ["main..HEAD"]);
        let args = diff_args(parse(&["v6.0", "v6.1", "--stat"]));
        assert_eq!(args.revs, ["v6.0", "v6.1"]);
        assert!(args.common.stat);
        assert!(diff_args(parse(&["--staged"])).staged);
        assert!(diff_args(parse(&["--unstaged"])).unstaged);
        assert!(parse(&[]).command.is_none());
    }

    #[test]
    fn unstaged_and_all_take_no_revisions_or_other_sides() {
        assert!(diff_args(parse(&["--all"])).all);
        for args in [
            &["--unstaged", "HEAD"][..],
            &["--unstaged", "--staged"],
            &["--all", "HEAD"],
            &["--all", "--staged"],
            &["--all", "--unstaged"],
            &["--all", "--merge-base"],
            &["--all", "--color-only"],
        ] {
            let mut full = vec![OsString::from("dffy")];
            full.extend(args.iter().map(OsString::from));
            assert!(
                Cli::try_parse_from(with_default_subcommand(full)).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn explicit_diff_takes_revisions_named_like_subcommands() {
        assert_eq!(diff_args(parse(&["diff", "config"])).revs, ["config"]);
        assert!(matches!(
            parse(&["config"]).command,
            Some(Command::Config {
                default: false,
                markdown: None
            })
        ));
    }

    #[test]
    fn global_options_work_before_and_after_subcommands() {
        let cli = parse(&["-C", "repo", "show", "HEAD~1", "--color=never"]);
        assert_eq!(cli.global.dir, Some(PathBuf::from("repo")));
        assert_eq!(cli.global.color, Some(ColorWhen::Never));
        let Some(Command::Show(show)) = cli.command else {
            panic!("expected show");
        };
        assert_eq!(show.rev, "HEAD~1");
        let cli = parse(&["--theme", "mono", "doctor"]);
        assert_eq!(cli.global.theme.as_deref(), Some("mono"));
        let cli = parse(&["-Crepo", "--paging=never", "v6.0..v6.1"]);
        assert_eq!(cli.global.dir, Some(PathBuf::from("repo")));
        assert_eq!(diff_args(cli).revs, ["v6.0..v6.1"]);
    }

    #[test]
    fn context_and_layout_flags_share_unified_like_git() {
        let args = diff_args(parse(&["-U5", "--", "src", "lib.rs"]));
        assert_eq!(args.common.view.context(), Some(Context::Lines(5)));
        assert!(!args.common.view.unified_view());
        assert_eq!(
            args.common.paths,
            [PathBuf::from("src"), PathBuf::from("lib.rs")]
        );
        let args = diff_args(parse(&["--unified=1"]));
        assert_eq!(args.common.view.context(), Some(Context::Lines(1)));
        assert!(!args.common.view.unified_view());
        let args = diff_args(parse(&["--unified=full"]));
        assert_eq!(args.common.view.context(), Some(Context::Full));
        assert!(!args.common.view.unified_view());
        let args = diff_args(parse(&["-Ufull"]));
        assert_eq!(args.common.view.context(), Some(Context::Full));
        let args = diff_args(parse(&["--unified", "HEAD"]));
        assert!(args.common.view.unified_view());
        assert_eq!(args.revs, ["HEAD"]);
    }

    #[test]
    fn stdin_dash_is_a_revision_argument() {
        assert_eq!(diff_args(parse(&["-"])).revs, ["-"]);
    }

    #[test]
    fn summary_flags_are_exclusive() {
        let args = ["dffy", "--stat", "--name-only"]
            .map(OsString::from)
            .to_vec();
        assert!(Cli::try_parse_from(with_default_subcommand(args)).is_err());
    }
}
