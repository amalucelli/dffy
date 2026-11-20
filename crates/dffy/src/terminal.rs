//! Which terminal dffy runs in, and the program that opens a new tab in it
//! at a directory, for the viewer's `open_terminal`, or one running a
//! program, for `fix_with_agent`'s coding agent. Read from the
//! environment alone, with nothing run to ask: the innermost terminal is
//! the one whose tab opens, as a multiplexer's variables sit beside those
//! of the terminal it runs in, which leak through.
//!
//! The directory and each word of a program are always arguments of their
//! own, never put inside a script's or a command's text, so nothing can
//! run as code; where a shell is handed a line, by a script or by tmux
//! given a single word, each word is quoted for it.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// Asks Ghostty (1.3 or later) for a tab whose shell starts in the first
/// argument.
const GHOSTTY_TAB: &str = "on run argv
\ttell application \"Ghostty\" to new tab with configuration {initial working directory:item 1 of argv}
end run";

/// iTerm2's scripting has no working directory for a new tab, so the tab
/// is told to `cd` there: the first argument, which the script quotes.
const ITERM_TAB: &str = "on run argv
\ttell application \"iTerm\"
\t\ttell current window
\t\t\tcreate tab with default profile
\t\t\ttell current session to write text (\"cd \" & quoted form of (item 1 of argv))
\t\tend tell
\tend tell
end run";

/// Asks Ghostty for a tab that runs the second argument and those after it
/// in the first, through `/bin/sh`, each quoted.
const GHOSTTY_RUN: &str = "on run argv
\tset cmdline to \"shell:\" & quoted form of (item 2 of argv)
\trepeat with i from 3 to count of argv
\t\tset cmdline to cmdline & \" \" & quoted form of (item i of argv)
\tend repeat
\ttell application \"Ghostty\" to new tab with configuration {initial working directory:item 1 of argv, command:cmdline}
end run";

/// [`ITERM_TAB`], the tab's shell then told to run the second argument and
/// those after it, each quoted.
const ITERM_RUN: &str = "on run argv
\tset cmdline to \"cd \" & quoted form of (item 1 of argv) & \" &&\"
\trepeat with i from 2 to count of argv
\t\tset cmdline to cmdline & \" \" & quoted form of (item i of argv)
\tend repeat
\ttell application \"iTerm\"
\t\ttell current window
\t\t\tcreate tab with default profile
\t\t\ttell current session to write text cmdline
\t\tend tell
\tend tell
end run";

/// A program that opens a tab, or a window where that's all the terminal
/// offers from outside, starting in it the user's shell or a program.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Opener {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// What opens, in the terminal's words: `a tmux window`, `a Ghostty
    /// tab`.
    pub opens: String,
}

/// A terminal, or a multiplexer running in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Program {
    Rex,
    Herdr,
    Tmux,
    Zellij,
    WezTerm,
    Kitty,
    Ghostty,
    ITerm,
    AppleTerminal,
    Alacritty,
}

/// The opener for the terminal `env` names, or why there's none: no
/// terminal dffy knows, or one it knows but can't drive from here. A
/// terminal found but not drivable never falls through to the one around
/// it, which would open a tab somewhere other than where the user looks.
/// `macos` picks Ghostty's way in.
///
/// With a `command`, its first word an absolute path, the tab runs it
/// rather than the shell; a terminal that can start only a shell from
/// outside (herdr, Ghostty on Linux, Terminal.app, Alacritty) is refused.
#[expect(clippy::too_many_lines, reason = "a table: an arm per terminal")]
pub(crate) fn opener(
    env: impl Fn(&str) -> Option<OsString>,
    macos: bool,
    dir: &Path,
    command: &[OsString],
) -> Result<Opener, String> {
    let program = detect(&env).ok_or("dffy doesn't know how to open a tab in this terminal")?;
    let need = |name: &str, why: &str| {
        env(name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| why.to_owned())
    };
    let dir = dir.as_os_str();
    let s = OsStr::new;
    let command: Vec<&OsStr> = command.iter().map(OsString::as_os_str).collect();
    let running = !command.is_empty();
    // A command after `--`, which ends the opener's own options.
    let ended: Vec<&OsStr> = (running.then_some(s("--")).into_iter())
        .chain(command.iter().copied())
        .collect();
    let shell_only = |name: &str| -> Result<Opener, String> {
        Err(format!(
            "dffy can't start a program in a new {name} tab, only a shell"
        ))
    };
    Ok(match program {
        Program::Rex => {
            let session = need(
                "REX_SESSION",
                "Rex's session isn't known: REX_SESSION isn't set",
            )?;
            // Rex puts its directory first on PATH in its terminals, which a
            // startup file can undo; REX_BIN_DIR stays.
            let rex = (env("REX_BIN_DIR").filter(|d| !d.is_empty())).map_or_else(
                || "rex".into(),
                |d| Path::new(&d).join("rex").into_os_string(),
            );
            let args = ["--autostart=false", "window", "new", "-s"].map(s);
            let args = [&args[..], &[&*session, s("-c"), dir], &ended].concat();
            run(rex, &args, "a Rex window")
        }
        // A tab of herdr's starts a shell; a program would have to be typed
        // into it.
        Program::Herdr if running => return shell_only("herdr"),
        Program::Herdr => {
            let missing = "herdr's command or workspace isn't known: \
                           HERDR_BIN_PATH or HERDR_WORKSPACE_ID isn't set";
            let herdr = need("HERDR_BIN_PATH", missing)?;
            let workspace = need("HERDR_WORKSPACE_ID", missing)?;
            let args = ["tab", "create", "--workspace"].map(s);
            let args = [&args[..], &[&*workspace, s("--cwd"), dir, s("--focus")]].concat();
            run(herdr, &args, "a herdr tab")
        }
        // Given more than one word, tmux runs them without a shell; one it
        // hands to its default shell as a command line.
        Program::Tmux => {
            let quoted = match command[..] {
                [word] => vec![OsString::from_vec(quote(word.as_bytes()))],
                _ => command.iter().map(|&w| w.to_owned()).collect(),
            };
            let words: Vec<&OsStr> = quoted.iter().map(OsString::as_os_str).collect();
            run(
                "tmux",
                &[&[s("new-window"), s("-c"), dir], &words[..]].concat(),
                "a tmux window",
            )
        }
        Program::Zellij => run(
            "zellij",
            &[&[s("action"), s("new-tab"), s("--cwd"), dir], &ended[..]].concat(),
            "a zellij tab",
        ),
        Program::WezTerm => run(
            "wezterm",
            &[&[s("cli"), s("spawn"), s("--cwd"), dir], &ended[..]].concat(),
            "a WezTerm tab",
        ),
        Program::Kitty => {
            // Without a socket, `kitten @` talks to kitty through the
            // terminal, which is dffy's screen.
            let to = need(
                "KITTY_LISTEN_ON",
                "kitty's remote control isn't on: set allow_remote_control and listen_on",
            )?;
            let args = [s("@"), s("--to"), &*to, s("launch"), s("--type=tab")];
            run(
                "kitten",
                &[&args[..], &[s("--cwd"), dir], &command[..]].concat(),
                "a kitty tab",
            )
        }
        Program::Ghostty if macos => {
            let script = if running { GHOSTTY_RUN } else { GHOSTTY_TAB };
            run(
                "osascript",
                &[&[s("-e"), s(script), dir], &command[..]].concat(),
                "a Ghostty tab",
            )
        }
        Program::Ghostty if running => return shell_only("Ghostty"),
        Program::Ghostty => {
            // Ghostty's options take their value after `=` only; the
            // directory is still the whole rest of the argument.
            let mut working = OsString::from("--working-directory=");
            working.push(dir);
            run(
                "ghostty",
                &[s("+new-window"), &*working],
                "a Ghostty window",
            )
        }
        Program::ITerm => {
            let script = if running { ITERM_RUN } else { ITERM_TAB };
            run(
                "osascript",
                &[&[s("-e"), s(script), dir], &command[..]].concat(),
                "an iTerm2 tab",
            )
        }
        Program::AppleTerminal if running => return shell_only("Terminal"),
        Program::AppleTerminal => run("open", &[s("-a"), s("Terminal"), dir], "a Terminal window"),
        Program::Alacritty if running => return shell_only("Alacritty"),
        Program::Alacritty => run(
            "alacritty",
            &[s("msg"), s("create-window"), s("--working-directory"), dir],
            "an Alacritty window",
        ),
    })
}

fn run(program: impl Into<OsString>, args: &[&OsStr], opens: &str) -> Opener {
    Opener {
        program: program.into(),
        args: args.iter().map(|&a| a.to_owned()).collect(),
        opens: opens.to_owned(),
    }
}

/// The innermost terminal `env` names. A multiplexer names itself in
/// `TERM_PROGRAM`, but zellij doesn't, leaving the outer terminal's there:
/// so `ZELLIJ` comes before the names of terminals zellij runs in, and
/// after those of multiplexers that run in zellij or zellij in them, which
/// the environment can't tell apart. Then each terminal's own variables,
/// for a `TERM_PROGRAM` that's missing or unknown, multiplexers first.
fn detect(env: impl Fn(&str) -> Option<OsString>) -> Option<Program> {
    let term_program = env("TERM_PROGRAM");
    let named = |names: &[(&str, Program)]| {
        let term_program = term_program.as_deref()?;
        names
            .iter()
            .find(|(n, _)| term_program == *n)
            .map(|&(_, p)| p)
    };
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    named(&[
        ("tmux", Program::Tmux),
        ("herdr", Program::Herdr),
        ("rex", Program::Rex),
    ])
    .or_else(|| set("ZELLIJ").then_some(Program::Zellij))
    .or_else(|| {
        named(&[
            ("WezTerm", Program::WezTerm),
            ("ghostty", Program::Ghostty),
            ("iTerm.app", Program::ITerm),
            ("Apple_Terminal", Program::AppleTerminal),
        ])
    })
    .or_else(|| {
        [
            ("REX_SESSION", Program::Rex),
            ("HERDR_ENV", Program::Herdr),
            ("TMUX", Program::Tmux),
            ("WEZTERM_PANE", Program::WezTerm),
            ("KITTY_WINDOW_ID", Program::Kitty),
            ("GHOSTTY_RESOURCES_DIR", Program::Ghostty),
            ("ITERM_SESSION_ID", Program::ITerm),
            ("ALACRITTY_SOCKET", Program::Alacritty),
            ("ALACRITTY_WINDOW_ID", Program::Alacritty),
        ]
        .into_iter()
        .find(|(name, _)| set(name))
        .map(|(_, p)| p)
    })
}

/// `cd` to `dir` as a shell reads it.
pub(crate) fn cd(dir: &Path) -> Vec<u8> {
    let mut out = b"cd ".to_vec();
    out.extend(quote(dir.as_os_str().as_bytes()));
    out
}

/// `word` as a shell reads it: single-quoted unless it's only characters
/// no shell treats specially. A quote or a backslash goes outside the
/// quotes, escaped, as fish reads `\\` and `\'` inside single quotes as
/// escapes where sh, bash and zsh don't; so all four read it the same.
pub(crate) fn quote(word: &[u8]) -> Vec<u8> {
    let plain = !word.is_empty()
        && (word.iter()).all(|&b| b.is_ascii_alphanumeric() || b"/._-+,:@".contains(&b));
    if plain {
        return word.to_vec();
    }
    let mut out = vec![b'\''];
    for &b in word {
        match b {
            b'\'' => out.extend_from_slice(b"'\\''"),
            b'\\' => out.extend_from_slice(b"'\\\\'"),
            _ => out.push(b),
        }
    }
    out.push(b'\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = "/src/my repo";

    /// The argv a fake environment of `vars` opens [`DIR`] with, its
    /// arguments parted by `|`, or why it can't.
    fn argv(vars: &[(&str, &str)], macos: bool) -> Result<String, String> {
        argv_running(vars, macos, &[])
    }

    /// [`argv`], running `command` in the tab.
    fn argv_running(
        vars: &[(&str, &str)],
        macos: bool,
        command: &[&str],
    ) -> Result<String, String> {
        let env = |name: &str| {
            (vars.iter())
                .find(|(n, _)| *n == name)
                .map(|(_, v)| OsString::from(v))
        };
        let command: Vec<OsString> = command.iter().map(OsString::from).collect();
        let opened = opener(env, macos, Path::new(DIR), &command)?;
        Ok(std::iter::once(&opened.program)
            .chain(&opened.args)
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("|"))
    }

    fn ok(vars: &[(&str, &str)], macos: bool) -> String {
        argv(vars, macos).unwrap_or_else(|why| panic!("{vars:?}: {why}"))
    }

    #[test]
    fn each_multiplexer_opens_with_the_directory_as_an_argument_of_its_own() {
        let rex = [
            ("TERM_PROGRAM", "rex"),
            ("REX_SESSION", "s1"),
            ("REX_BIN_DIR", "/rx/bin"),
        ];
        assert_eq!(
            ok(&rex, true),
            "/rx/bin/rex|--autostart=false|window|new|-s|s1|-c|/src/my repo"
        );
        assert_eq!(
            ok(&[("REX_SESSION", "s1")], true),
            "rex|--autostart=false|window|new|-s|s1|-c|/src/my repo"
        );
        let herdr = [
            ("TERM_PROGRAM", "herdr"),
            ("HERDR_ENV", "1"),
            ("HERDR_BIN_PATH", "/opt/bin/herdr"),
            ("HERDR_WORKSPACE_ID", "w6T"),
        ];
        assert_eq!(
            ok(&herdr, true),
            "/opt/bin/herdr|tab|create|--workspace|w6T|--cwd|/src/my repo|--focus"
        );
        let tmux = [
            ("TERM_PROGRAM", "tmux"),
            ("TMUX", "/tmp/tmux-1/default,1,0"),
        ];
        assert_eq!(ok(&tmux, false), "tmux|new-window|-c|/src/my repo");
        assert_eq!(
            ok(&[("ZELLIJ", "0")], false),
            "zellij|action|new-tab|--cwd|/src/my repo"
        );
    }

    #[test]
    fn each_terminal_opens_with_the_directory_as_an_argument_of_its_own() {
        let wezterm = [("TERM_PROGRAM", "WezTerm"), ("WEZTERM_PANE", "3")];
        assert_eq!(ok(&wezterm, false), "wezterm|cli|spawn|--cwd|/src/my repo");
        let kitty = [("KITTY_WINDOW_ID", "1"), ("KITTY_LISTEN_ON", "unix:/k")];
        assert_eq!(
            ok(&kitty, false),
            "kitten|@|--to|unix:/k|launch|--type=tab|--cwd|/src/my repo"
        );
        assert_eq!(
            ok(&[("TERM_PROGRAM", "ghostty")], true),
            format!("osascript|-e|{GHOSTTY_TAB}|{DIR}")
        );
        assert_eq!(
            ok(&[("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty")], false),
            "ghostty|+new-window|--working-directory=/src/my repo"
        );
        assert_eq!(
            ok(&[("TERM_PROGRAM", "iTerm.app")], true),
            format!("osascript|-e|{ITERM_TAB}|{DIR}")
        );
        assert_eq!(
            ok(&[("TERM_PROGRAM", "Apple_Terminal")], true),
            "open|-a|Terminal|/src/my repo"
        );
        assert_eq!(
            ok(&[("ALACRITTY_WINDOW_ID", "1")], false),
            "alacritty|msg|create-window|--working-directory|/src/my repo"
        );
    }

    /// The scripts take the directory from their arguments; nothing of it
    /// is in their text.
    #[test]
    fn the_scripts_read_the_directory_from_their_arguments() {
        for script in [GHOSTTY_TAB, ITERM_TAB, GHOSTTY_RUN, ITERM_RUN] {
            assert!(script.starts_with("on run argv\n"), "{script}");
            assert!(script.contains("item 1 of argv"), "{script}");
        }
        for script in [GHOSTTY_RUN, ITERM_RUN] {
            assert!(
                script.contains("quoted form of (item i of argv)"),
                "{script}"
            );
        }
    }

    /// A command runs in the new tab with each of its words an argument of
    /// its own, after the directory; a terminal with no way to start one
    /// there says why, for the caller to run it another way.
    #[test]
    fn a_command_runs_in_the_new_tab_with_its_words_as_arguments() {
        let run = |vars: &[(&str, &str)], macos: bool| {
            argv_running(vars, macos, &["/bin/claude", "Address it"])
        };
        let ok = |vars: &[(&str, &str)], macos: bool| {
            run(vars, macos).unwrap_or_else(|why| panic!("{vars:?}: {why}"))
        };
        assert_eq!(
            ok(&[("REX_SESSION", "s1")], true),
            "rex|--autostart=false|window|new|-s|s1|-c|/src/my repo|--|/bin/claude|Address it"
        );
        assert_eq!(
            ok(&[("TMUX", "t")], false),
            "tmux|new-window|-c|/src/my repo|/bin/claude|Address it"
        );
        assert_eq!(
            ok(&[("ZELLIJ", "0")], false),
            "zellij|action|new-tab|--cwd|/src/my repo|--|/bin/claude|Address it"
        );
        assert_eq!(
            ok(&[("WEZTERM_PANE", "3")], false),
            "wezterm|cli|spawn|--cwd|/src/my repo|--|/bin/claude|Address it"
        );
        let kitty = [("KITTY_WINDOW_ID", "1"), ("KITTY_LISTEN_ON", "unix:/k")];
        assert_eq!(
            ok(&kitty, false),
            "kitten|@|--to|unix:/k|launch|--type=tab|--cwd|/src/my repo|/bin/claude|Address it"
        );
        assert_eq!(
            ok(&[("TERM_PROGRAM", "ghostty")], true),
            format!("osascript|-e|{GHOSTTY_RUN}|{DIR}|/bin/claude|Address it")
        );
        assert_eq!(
            ok(&[("TERM_PROGRAM", "iTerm.app")], true),
            format!("osascript|-e|{ITERM_RUN}|{DIR}|/bin/claude|Address it")
        );
        let herdr = [
            ("TERM_PROGRAM", "herdr"),
            ("HERDR_BIN_PATH", "/opt/bin/herdr"),
            ("HERDR_WORKSPACE_ID", "w6T"),
        ];
        for vars in [
            &herdr[..],
            &[("GHOSTTY_RESOURCES_DIR", "/g")],
            &[("TERM_PROGRAM", "Apple_Terminal")],
            &[("ALACRITTY_WINDOW_ID", "1")],
        ] {
            let why = run(vars, false).expect_err("no command there");
            assert!(why.contains("can't start a program"), "{vars:?}: {why}");
        }
    }

    /// The innermost terminal opens the tab, whichever outer one's
    /// variables leak through.
    #[test]
    fn the_innermost_terminal_is_the_one_found() {
        let first = |vars: &[(&str, &str)]| {
            let argv = ok(vars, true);
            argv.split('|').next().unwrap_or_default().to_owned()
        };
        let herdr_in_ghostty = [
            ("TERM_PROGRAM", "herdr"),
            ("HERDR_ENV", "1"),
            ("HERDR_BIN_PATH", "herdr"),
            ("HERDR_WORKSPACE_ID", "w"),
            ("GHOSTTY_RESOURCES_DIR", "/g"),
        ];
        assert_eq!(first(&herdr_in_ghostty), "herdr");
        // zellij leaves the outer terminal's TERM_PROGRAM in place.
        assert_eq!(
            first(&[("TERM_PROGRAM", "ghostty"), ("ZELLIJ", "0")]),
            "zellij"
        );
        // tmux in zellij and zellij in tmux look the same: tmux it is.
        let tmux_and_zellij = [("TERM_PROGRAM", "tmux"), ("ZELLIJ", "0"), ("TMUX", "t")];
        assert_eq!(first(&tmux_and_zellij), "tmux");
        // tmux in kitty, which sets no TERM_PROGRAM of its own.
        assert_eq!(first(&[("TMUX", "t"), ("KITTY_WINDOW_ID", "1")]), "tmux");
        // A TERM_PROGRAM dffy doesn't know falls to the variables.
        assert_eq!(first(&[("TERM_PROGRAM", "vscode"), ("TMUX", "t")]), "tmux");
    }

    /// A terminal found that can't be driven says why, rather than opening
    /// a tab in the terminal around it.
    #[test]
    fn a_terminal_that_cant_be_driven_says_why() {
        let why = |vars: &[(&str, &str)]| argv(vars, true).expect_err("refused");
        let kitty_in_ghostty = [("KITTY_WINDOW_ID", "1"), ("GHOSTTY_RESOURCES_DIR", "/g")];
        assert!(why(&kitty_in_ghostty).contains("listen_on"));
        let herdr_in_ghostty = [("TERM_PROGRAM", "herdr"), ("GHOSTTY_RESOURCES_DIR", "/g")];
        assert!(why(&herdr_in_ghostty).contains("HERDR_BIN_PATH"));
        assert!(why(&[("TERM_PROGRAM", "rex")]).contains("REX_SESSION"));
        assert_eq!(
            why(&[("TERM", "xterm-256color")]),
            "dffy doesn't know how to open a tab in this terminal"
        );
    }

    #[test]
    fn cd_quotes_what_a_shell_would_read_otherwise() {
        let cd = |dir: &str| String::from_utf8(cd(Path::new(dir))).expect("utf-8");
        assert_eq!(cd("/src/dffy-2.0_x"), "cd /src/dffy-2.0_x");
        assert_eq!(cd("/src/my repo"), "cd '/src/my repo'");
        assert_eq!(cd("/src/it's $HOME"), r"cd '/src/it'\''s $HOME'");
        // A backslash is outside the quotes, where fish reads it as sh does.
        assert_eq!(cd(r"/src/a\b"), r"cd '/src/a'\\'b'");
    }

    /// tmux runs a single word through the user's shell, so it's quoted;
    /// more than one run without a shell.
    #[test]
    fn a_one_word_command_in_tmux_is_quoted_for_its_shell() {
        let tmux = [("TMUX", "t")];
        let one = argv_running(&tmux, false, &["/opt/my tools/amp"]).expect("tmux");
        assert_eq!(one, "tmux|new-window|-c|/src/my repo|'/opt/my tools/amp'");
        let plain = argv_running(&tmux, false, &["/bin/amp"]).expect("tmux");
        assert_eq!(plain, "tmux|new-window|-c|/src/my repo|/bin/amp");
    }
}
