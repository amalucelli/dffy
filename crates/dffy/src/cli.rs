//! Command-line interface. `diff` is the default subcommand, so
//! `dffy main..HEAD` means `dffy diff main..HEAD`; a revision named like a
//! subcommand needs the explicit form (`dffy diff config`). Diff flags mirror
//! `git diff`.
//!
//! The review plugin's command (`dffy pr`) is a subcommand only when a
//! review plugin is set and describes it, so it's added at run time by
//! [`command`]; without one, `pr` is a revision like any other.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::config::{ColorWhen, Context, Paging};
use crate::plugin::Review;
use crate::plugin::review::Naming;

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
const GLOBAL_WITH_VALUE: [&str; 6] = [
    "-C",
    "--color",
    "--paging",
    "--theme",
    "--comments",
    "--review",
];

/// dffy's command line, with the review plugin's command when `review`
/// describes one.
pub(crate) fn command(review: Option<&Naming>) -> clap::Command {
    with_review(Cli::command(), review)
}

/// `command` with the review plugin's command when `review` describes one.
pub(crate) fn with_review(command: clap::Command, review: Option<&Naming>) -> clap::Command {
    let Some(Naming {
        command: name,
        noun,
        nouns,
        ..
    }) = review
    else {
        return command;
    };
    command.subcommand(
        clap::Command::new(name.clone())
            .about(format!(
                "Review a {noun} in the viewer, through the review plugin"
            ))
            .long_about(format!(
                "Review a {noun} in the viewer, through the review plugin (the only \
                 dffy-review-<name> on PATH, or the one review = <name> in the config or \
                 --review <name> names), which names this command.\n\n\
                 With an id, it opens that {noun}: its diff from base to head, with its \
                 review threads, as soon as the head is in the clone. Without one, it opens \
                 the current branch's {noun}, or lists the {nouns} to pick from. \
                 Experimental; see docs/plugins.md."
            ))
            .arg(clap::Arg::new("id").help(format!("The {noun}'s id"))),
    )
}

/// Whether `name` is one of `command`'s subcommands or their aliases, or
/// `help`; for dffy's own, a name a review plugin's command can't be.
pub(crate) fn is_subcommand(command: &clap::Command, name: &str) -> bool {
    name == "help"
        || command
            .get_subcommands()
            .any(|s| s.get_name() == name || s.get_all_aliases().any(|a| a == name))
}

/// Parses `args` with `command`, from [`command`]: its review command, if
/// any, comes out as [`Command::Review`].
pub(crate) fn parse(command: clap::Command, args: &[OsString]) -> Result<Cli, clap::Error> {
    let mut matches = command.try_get_matches_from(args)?;
    let review = match matches.subcommand_name() {
        // Matches name a subcommand by its name, never an alias, so the
        // derive's list answers without building the command again.
        Some(name) if !Command::has_subcommand(name) => matches
            .remove_subcommand()
            .map(|(_, sub)| sub.get_one::<String>("id").cloned()),
        _ => None,
    };
    let mut cli = Cli::from_arg_matches(&matches)?;
    if let Some(id) = review {
        cli.command = Some(Command::Review { id });
    }
    Ok(cli)
}

/// `--review` and `--no-cache`, read before parsing: whether there's a
/// review command to parse depends on them. A `--review` value that isn't
/// one is left for clap to refuse.
pub(crate) fn early_flags(args: &[OsString]) -> (Option<Review>, bool) {
    let (mut review, mut no_cache) = (None, false);
    let mut iter = args.iter().skip(1).filter_map(|a| a.to_str());
    while let Some(arg) = iter.next() {
        match arg {
            "--" => break,
            "--no-cache" => no_cache = true,
            "--review" => review = iter.next().map(str::to_owned),
            _ => {
                if let Some(name) = arg.strip_prefix("--review=") {
                    review = Some(name.to_owned());
                }
            }
        }
    }
    (review.and_then(|value| value.parse().ok()), no_cache)
}

/// Inserts `diff` before the first argument that is neither a global option
/// nor one of `command`'s subcommands, so `dffy -C repo main..HEAD --stat`
/// parses as `dffy -C repo diff main..HEAD --stat`. clap's own way to have
/// arguments without a subcommand turns global options off for the
/// subcommands.
pub(crate) fn with_default_subcommand(
    command: &clap::Command,
    mut args: Vec<OsString>,
) -> Vec<OsString> {
    let at = first_command(&args)
        .filter(|(_, arg)| {
            !matches!(*arg, "-h" | "--help" | "-V" | "--version") && !is_subcommand(command, arg)
        })
        .map(|(i, _)| i);
    if let Some(i) = at {
        args.insert(i, OsString::from("diff"));
    }
    args
}

/// Whether parsing `args` with `command` needs the review plugin's command
/// first: help lists it, and a first word that isn't one of `command`'s
/// subcommands may be it, unless no command could be named so
/// (`main..HEAD`, `HEAD~3`, `src/lib.rs`). Every other run that uses the
/// plugin's description asks for it after parsing.
pub(crate) fn parses_review(command: &clap::Command, args: &[OsString]) -> bool {
    first_command(args).is_some_and(|(_, arg)| {
        matches!(arg, "-h" | "--help" | "help")
            || (!is_subcommand(command, arg) && Naming::can_be_command(arg))
    })
}

/// The first argument that isn't a global option, and where it is.
fn first_command(args: &[OsString]) -> Option<(usize, &str)> {
    let mut i = 1;
    while let Some(arg) = args.get(i) {
        let arg = arg.to_str().unwrap_or_default();
        if GLOBAL_WITH_VALUE.contains(&arg) {
            i += 2;
        } else if arg == "--no-cache"
            || GLOBAL_WITH_VALUE.iter().any(|g| {
                arg.strip_prefix(g)
                    .is_some_and(|rest| rest.starts_with('=') || (*g == "-C" && !rest.is_empty()))
            })
        {
            i += 1;
        } else {
            return Some((i, arg));
        }
    }
    None
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
    /// Review comments file; the default is dffy/comments.json in the git
    /// directory the repository's worktrees share
    #[arg(long, value_name = "file", global = true)]
    pub comments: Option<PathBuf>,
    /// Review plugin, dffy-review-<name>, over the config's review for this
    /// run: auto takes the only one on PATH, none turns them off
    #[arg(long, value_name = "name", global = true, value_parser = str::parse::<Review>)]
    pub review: Option<Review>,
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
    /// List the review comments, or change them with apply
    #[command(long_about = "List the review comments of --comments <file>, of the \
        review session the viewer has open with --session, or of dffy/comments.json \
        in the repository's shared git directory: the current branch's threads and \
        those on no branch, unless --branch or --all-branches says otherwise.\n\n\
        With --json, print the whole file in its format; with --markdown, the \
        threads with their lines. `dffy comments apply` reads a JSON list of \
        operations on stdin, applies them all or none, and prints the ids they \
        touched, with --as <name> for an agent's comments; `dffy comments rm --resolved` removes resolved threads; `dffy \
        comments prompt <id>` prints a thread as a prompt for a coding agent. See \
        docs/comments.md.")]
    Comments(CommentsArgs),
    /// The review plugin's command, which [`command`] adds and [`parse`]
    /// reads.
    #[command(skip)]
    Review { id: Option<String> },
    /// Take a checkpoint of the working tree and print its number
    #[command(
        long_about = "Take a checkpoint of the working tree and print its number.\n\n\
        A checkpoint keeps the files plain dffy lists (staged and unstaged changes, and \
        untracked files git doesn't ignore) under a local ref of this worktree, \
        refs/worktree/dffy/checkpoints/<n>, without touching the index, HEAD or the files. \
        dffy --since-checkpoint then shows only what changed since the latest, and \
        --since-checkpoint=<n> since that one. The last 5 are kept. See docs/usage.md."
    )]
    Checkpoint,
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
    /// Check git, the repository, watch limits, the terminal, the config, plugins and agents
    Doctor {
        /// Print every check and measurement as one JSON object
        #[arg(long)]
        json: bool,
        /// Print each check on a line of its own, with all it found
        #[arg(long, conflicts_with = "json")]
        verbose: bool,
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
        directory. changed_files, file_diff and diff return the objects of dffy --json, \
        and list_comments the review comments, a page at a time, thread_prompt one \
        thread as a prompt to answer, and reply_to_thread a reply in one; open_in_viewer, \
        focus_thread, viewer_selection and apply_suggestion drive the viewer running there. Register it \
        with an agent, as in: claude mcp add dffy -- dffy mcp. See docs/mcp.md."
    )]
    Mcp,
    /// Drive a running viewer: show a file at a line, focus a thread, read the selection, apply a suggestion
    #[command(long_about = "Drive a running viewer through dffyd's control API.\n\n\
        Each command goes to the viewer running in this working tree (or -C <path>'s), \
        or to --view <id>. Paths are relative to the current directory. --json prints \
        the protocol's result. See docs/control.md.")]
    Ctl(CtlArgs),
    /// Teach coding agents to use dffy: install, check or remove its skill, or print the guide
    #[command(long_about = "Teach coding agents to use dffy.\n\n\
        setup writes a small skill, dffy/SKILL.md, where agents read skills: \
        ~/.claude/skills for Claude Code and ~/.agents/skills for Codex, Pi and Amp, \
        or with --project the same directories at the repository's root. The skill \
        tells the agent to run dffy agents show, which prints the guide for this \
        dffy. status says whether each place holds the current skill, and remove \
        takes back the skills setup wrote. See docs/agents.md.")]
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },
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

#[derive(Debug, Args)]
#[expect(clippy::struct_excessive_bools, reason = "each flag is its own switch")]
pub(crate) struct CommentsArgs {
    /// Print the whole comments file as JSON (docs/comments.md)
    #[arg(long, conflicts_with_all = ["markdown", "open", "resolved", "path", "branch", "all_branches"])]
    pub json: bool,
    /// Print the threads as Markdown, with the lines they're on
    #[arg(long)]
    pub markdown: bool,
    /// The comments of the review session the viewer has open, instead of the local ones
    #[arg(long)]
    pub session: bool,
    /// Only open threads
    #[arg(long, conflicts_with = "resolved")]
    pub open: bool,
    /// Only resolved threads
    #[arg(long)]
    pub resolved: bool,
    /// Only the threads on this file, relative to the repository root; a renamed file's old path works too
    #[arg(long, value_name = "path")]
    pub path: Option<String>,
    #[command(flatten)]
    pub branches: BranchArgs,
    #[command(subcommand)]
    pub action: Option<CommentsAction>,
}

impl CommentsArgs {
    /// Whether a flag that only a listing reads is given.
    pub(crate) fn lists(&self) -> bool {
        self.json
            || self.markdown
            || self.open
            || self.resolved
            || self.path.is_some()
            || self.branches.branch.is_some()
            || self.branches.all_branches
    }
}

/// Which branches' threads a comments command takes; by default the
/// current branch's and those on no branch.
#[derive(Debug, Clone, Default, Args)]
pub(crate) struct BranchArgs {
    /// The threads on this branch and those on none, instead of the current branch's
    #[arg(long, value_name = "name", conflicts_with = "all_branches")]
    pub branch: Option<String>,
    /// Every branch's threads
    #[arg(long)]
    pub all_branches: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum CommentsAction {
    /// Apply a JSON list of operations from stdin, all or none, and print the ids they touched
    Apply {
        /// The author of the comments it adds that name none, as an agent gives its own name; without it they're yours
        #[arg(long = "as", value_name = "name", value_parser = author_name)]
        author: Option<String>,
    },
    /// Add the comments of another comments file that this one doesn't have, by id or ref
    Import {
        /// The comments file to take them from
        #[arg(value_name = "file")]
        from: PathBuf,
    },
    /// Remove the resolved threads, with their replies, of the current branch and of none
    Rm {
        /// Remove the resolved threads; required
        #[arg(long, required = true)]
        resolved: bool,
        #[command(flatten)]
        branches: BranchArgs,
    },
    /// Print a thread as a prompt for a coding agent: its comments, its lines and the file's changes near them
    Prompt {
        /// The id of any comment in the thread
        #[arg(
            value_name = "id",
            required_unless_present = "open",
            conflicts_with = "open"
        )]
        id: Option<String>,
        /// Every open thread of yours on the current branch in one prompt, each under its id: those you started or replied in
        #[arg(long)]
        open: bool,
        /// With --open, the threads' ids, how many on other branches were left out, and the prompt, as one JSON object
        #[arg(long, requires = "open")]
        json: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct CtlArgs {
    /// The view to drive, by the id dffy ctl views lists; else the one viewer in this working tree
    #[arg(long, value_name = "id", global = true)]
    pub view: Option<u64>,
    /// Print the protocol's JSON result instead of text
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub action: CtlAction,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum CtlAction {
    /// List the running viewers
    Views,
    /// Show a file of the diff, at a new-side line or, with :old, an old-side one
    Open {
        #[arg(value_name = "path[:line[:old]]")]
        at: String,
    },
    /// Show and focus a comment thread, named by any of its comments' ids
    Focus {
        #[arg(value_name = "thread")]
        thread: String,
    },
    /// Print the lines selected in the viewer, after their place
    Selection,
    /// Print the file shown, its top line and what the viewer compares
    State,
    /// Add a comment on a line, as c in the viewer would
    Comment {
        #[arg(value_name = "path:line[:old]")]
        at: String,
        /// The comment, its words joined with spaces
        #[arg(required = true, value_name = "text")]
        text: Vec<String>,
    },
    /// Put a thread's suggestion in the working tree, as s on the thread in the viewer would
    ApplySuggestion {
        /// A comment with a suggestion, or any comment of its thread for the last one with one
        #[arg(value_name = "id")]
        id: String,
    },
    /// Print the protocol's JSON Schema
    Schema,
}

/// What a comment's author can be named: what MCP makes of a client's
/// name, refused rather than replaced, and never `you`, which is how your
/// own comments show.
fn author_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("you") {
        return Err("give your own name, not empty or `you`, which means the user".to_owned());
    }
    if name.chars().any(char::is_control) {
        return Err("a name can't hold control characters".to_owned());
    }
    if name.chars().count() > 64 {
        return Err("a name is at most 64 characters".to_owned());
    }
    Ok(name.to_owned())
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum AgentsAction {
    /// Write the dffy skill where agents read skills
    Setup {
        #[command(flatten)]
        at: SkillArgs,
        /// Say what it would write, and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Replace a dffy/SKILL.md that dffy didn't write
        #[arg(long)]
        force: bool,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Say whether each place holds the current skill, an outdated one, another's, or none
    Status {
        #[command(flatten)]
        at: SkillArgs,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove the skills setup wrote; another dffy/SKILL.md stays
    Remove {
        #[command(flatten)]
        at: SkillArgs,
        /// Say what it would remove, and remove nothing
        #[arg(long)]
        dry_run: bool,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print the guide the skill points agents to: how to work with this dffy
    Show,
}

/// Where `dffy agents` looks.
#[derive(Debug, Clone, Args)]
pub(crate) struct SkillArgs {
    /// Only the place this agent reads; repeatable. Without it, every agent's
    #[arg(long, value_enum, value_name = "agent")]
    pub agent: Vec<crate::agents::Agent>,
    /// The repository's own skill directories, at its root, instead of the home directory's
    #[arg(long)]
    pub project: bool,
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
    /// Compare the latest checkpoint, or checkpoint N, with the working tree: only what changed since `dffy checkpoint` printed N
    #[arg(
        long,
        value_name = "N",
        num_args = 0..=1,
        require_equals = true,
        conflicts_with_all = ["revs", "staged", "unstaged", "all", "merge_base", "no_index", "color_only"]
    )]
    #[expect(
        clippy::option_option,
        reason = "clap's optional-value form: absent, bare --since-checkpoint, or --since-checkpoint=<n>"
    )]
    pub since_checkpoint: Option<Option<u64>>,
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
            "focus", "paths", "show_comments",
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
#[expect(clippy::struct_excessive_bools, reason = "each flag is its own switch")]
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
    /// Print each review comment thread under the line it's on; with --json, add each file's annotations. The threads of --comments <file>, or the repository's on the current branch (docs/comments.md)
    #[arg(long, conflicts_with = "summary")]
    pub show_comments: bool,
    /// With --show-comments, the threads of the review session the viewer has open instead of the local ones
    #[arg(long, requires = "show_comments")]
    pub session: bool,
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
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        parse_with(None, args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    fn parse_with(review: Option<&Naming>, args: &[&str]) -> Result<Cli, clap::Error> {
        let mut full = vec![OsString::from("dffy")];
        full.extend(args.iter().map(OsString::from));
        let command = command(review);
        super::parse(command.clone(), &with_default_subcommand(&command, full))
    }

    fn merge_request() -> Naming {
        let answer =
            br#"{"protocol":1,"command":"mr","noun":"merge request","nouns":"merge requests"}"#;
        Naming::decode(&crate::plugin::json::parse(answer).expect("JSON"))
            .expect("an initialize answer")
    }

    /// With a review plugin that describes `mr`, `mr` is a subcommand that
    /// takes an optional id, whatever the plugin's ids look like, and the
    /// global options, and `pr` a revision; without one, neither is a
    /// subcommand.
    #[test]
    fn the_review_command_is_a_subcommand_only_when_described() {
        let mr = merge_request();
        command(Some(&mr)).debug_assert();
        let review = |args: &[&str]| match parse_with(Some(&mr), args) {
            Ok(Cli {
                command: Some(Command::Review { id }),
                global,
            }) => (id, global),
            other => panic!("{args:?}: {other:?}"),
        };
        assert_eq!(review(&["mr", "7"]).0.as_deref(), Some("7"));
        assert_eq!(review(&["mr", "I8f2c"]).0.as_deref(), Some("I8f2c"));
        assert_eq!(review(&["mr"]).0, None);
        let (id, global) = review(&["-C", "repo", "mr", "7", "--color=never"]);
        assert_eq!(id.as_deref(), Some("7"));
        assert_eq!(global.dir, Some(PathBuf::from("repo")));
        assert_eq!(global.color, Some(ColorWhen::Never));
        let (id, global) = review(&["--no-cache", "mr", "7"]);
        assert_eq!(id.as_deref(), Some("7"));
        assert!(global.no_cache);
        let revision = |review: Option<&Naming>, args: &[&str]| {
            diff_args(parse_with(review, args).unwrap_or_else(|e| panic!("{args:?}: {e}"))).revs
        };
        assert_eq!(revision(Some(&mr), &["pr", "7"]), ["pr", "7"]);
        assert_eq!(revision(None, &["pr", "7"]), ["pr", "7"]);
        assert_eq!(revision(None, &["mr"]), ["mr"]);
        assert_eq!(revision(Some(&mr), &["diff", "mr"]), ["mr"]);

        let help = |review| command(review).render_long_help().to_string();
        let described = help(Some(&mr));
        assert!(
            described.contains("mr           Review a merge request in the viewer"),
            "{described}"
        );
        assert!(!help(None).contains("Review a"), "{}", help(None));
    }

    #[test]
    fn a_review_command_cant_be_a_builtin_subcommand_or_alias() {
        let builtin = |name| is_subcommand(&Cli::command(), name);
        for name in [
            "diff", "show", "config", "doctor", "mcp", "agents", "man", "help",
        ] {
            assert!(builtin(name), "{name}");
        }
        assert!(!builtin("mr"));
        assert!(!builtin("pr"));
    }

    #[test]
    fn early_flags_are_read_before_parsing() {
        let early = |args: &[&str]| {
            let mut full = vec![OsString::from("dffy")];
            full.extend(args.iter().map(OsString::from));
            early_flags(&full)
        };
        let gitlab = Some(Review::Named("gitlab".to_owned()));
        assert_eq!(
            early(&["--review", "gitlab", "mr"]),
            (gitlab.clone(), false)
        );
        assert_eq!(early(&["mr", "--review=gitlab"]), (gitlab, false));
        assert_eq!(early(&["--no-cache", "mr"]), (None, true));
        assert_eq!(early(&["--review", "../evil", "mr"]), (None, false));
        assert_eq!(early(&["--", "--review", "gitlab"]), (None, false));
    }

    #[test]
    fn only_help_and_a_word_that_may_be_the_review_command_parse_with_it() {
        let parses = |args: &[&str]| {
            let mut full = vec![OsString::from("dffy")];
            full.extend(args.iter().map(OsString::from));
            parses_review(&Cli::command(), &full)
        };
        for args in [
            &["-C", "repo", "pr", "7"][..],
            &["--help"],
            &["help"],
            &["main"],
        ] {
            assert!(parses(args), "{args:?}");
        }
        for args in [
            &[][..],
            &["--no-cache"],
            &["main..HEAD"],
            &["HEAD~3"],
            &["src/lib.rs"],
            &["--", "readme"],
            &["--stat"],
            &["diff", "pr"],
            &["comments", "apply"],
        ] {
            assert!(!parses(args), "{args:?}");
        }
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
            assert!(parse_with(None, args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn explicit_diff_takes_revisions_named_like_subcommands() {
        assert_eq!(diff_args(parse(&["diff", "config"])).revs, ["config"]);
        assert_eq!(diff_args(parse(&["diff", "comments"])).revs, ["comments"]);
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
        let cli = parse(&["--comments", "pr.json", "main...HEAD"]);
        assert_eq!(cli.global.comments, Some(PathBuf::from("pr.json")));
        assert_eq!(diff_args(cli).revs, ["main...HEAD"]);
        // As dffy-review-github starts it.
        let cli = parse(&["--review", "github", "--comments", "pr.json", "a...b"]);
        assert_eq!(cli.global.review, Some(Review::Named("github".to_owned())));
        assert_eq!(diff_args(cli).revs, ["a...b"]);
        assert!(parse_with(None, &["--review", "../evil", "HEAD"]).is_err());
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
        assert!(parse_with(None, &["--stat", "--name-only"]).is_err());
    }
}
