use super::support::*;

/// The screen as text, the command panel's dimmed text as `<dim>…</>`.
fn panel_markup(app: &mut App) -> String {
    app.draw();
    let dim = app.theme.style(Role::Dim);
    let grid = app.screen.shown_grid();
    (0..grid.height())
        .map(|y| {
            let mut line = String::new();
            let mut in_dim = false;
            for cell in grid.row(y) {
                let dimmed = cell.style() == dim && cell.text() != " ";
                if dimmed != in_dim {
                    line.push_str(if dimmed { "<dim>" } else { "</>" });
                    in_dim = dimmed;
                }
                line.push_str(cell.text());
            }
            if in_dim {
                line.push_str("</>");
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tab the panel shows.
fn tab(app: &App) -> &'static str {
    app.commands.as_ref().map_or("", Commands::tab)
}

/// Whether the quick popup shows, rather than the full panel.
fn quick(app: &App) -> bool {
    app.commands.as_ref().is_some_and(Commands::quick)
}

/// The full panel's box alone, its rows between its borders.
fn panel(app: &mut App) -> String {
    boxed(app, "commands")
}

/// The box titled `title` alone, its rows between its borders.
fn boxed(app: &mut App, title: &str) -> String {
    let shown = screen(app);
    let lines: Vec<Vec<char>> = shown.lines().map(|l| l.chars().collect()).collect();
    let top = (lines.iter())
        .position(|l| (l.iter().collect::<String>()).contains(&format!("─ {title} ─")))
        .unwrap_or_else(|| panic!("{title}:\n{shown}"));
    let left = lines[top].iter().position(|&c| c == '┌').expect("corner");
    let right = left + (lines[top][left..].iter().position(|&c| c == '┐')).expect("corner");
    (lines[top..].iter())
        .take_while(|l| l.get(left) != Some(&'└'))
        .map(|l| {
            l[left..=right]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A viewer of a review, at `size`, its thread `d1` focused.
fn on_thread(dir: &tempfile::TempDir, size: (usize, usize)) -> App {
    let mut app = reviewed(dir, &review_json("src/zz9.rs"), size);
    for _ in 0..5 {
        if focused(&app) == Some("d1") {
            break;
        }
        key(&mut app, 'm');
    }
    assert_eq!(focused(&app), Some("d1"));
    app
}

/// `space` opens the quick popup: on a focused thread, its actions under
/// "On this thread", then View and Go, and a footer pointing to the rest.
/// Each row shows one key, the one from outside the panel when there's
/// one: `f` fixes with an agent, not `& f`; both run it.
#[test]
fn space_opens_a_quick_popup_with_the_threads_actions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir, (140, 40));
    key(&mut app, ' ');
    assert!(quick(&app));
    let popup = boxed(&mut app, "commands");
    let line = |wanted: &str| {
        (popup.lines())
            .position(|l| l.contains(wanted))
            .unwrap_or_else(|| panic!("{wanted}:\n{popup}"))
    };
    assert!(line("On this thread") < line("Reply"));
    assert!(line("Reply") < line("View"));
    assert!(line("View") < line("Syntax diff"));
    assert!(line("Syntax diff") < line("Go"));
    assert!(line("Go") < line("Comments panel"));
    assert!(popup.contains("│  f  Fix with agent"), "{popup}");
    assert!(
        !popup.contains('&') && !popup.contains(" Here  View"),
        "{popup}"
    );
    insta::assert_snapshot!("commands_quick_thread", popup);
    let phone_dir = tempfile::tempdir().expect("tempdir");
    let mut phone = on_thread(&phone_dir, (50, 40));
    key(&mut phone, ' ');
    let popup = boxed(&mut phone, "commands");
    insta::assert_snapshot!("commands_quick_thread_narrow", popup);
    let fix = app
        .commands
        .as_mut()
        .map(|c| c.key(Key::Char('&'), Mods::NONE));
    let fix_runs = crate::tui::commands::Outcome::Run {
        action: Act::FixWithAgent,
        back: false,
    };
    assert_eq!(fix, Some(fix_runs), "the panel's key runs it too");
    key(&mut app, 'r');
    assert!(app.commands.is_none());
    assert!(matches!(app.composing, Some(Compose::Reply(ref root)) if root == "d1"));
}

/// A screen too short for the whole quick popup drops its View group
/// before Go, and the cursor and keys follow the rows shown.
#[test]
fn a_short_screen_drops_the_quick_popups_view_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir, (140, 24));
    key(&mut app, ' ');
    let popup = boxed(&mut app, "commands");
    insta::assert_snapshot!("commands_quick_short", popup);
    assert!(
        !popup.contains("Syntax diff") && popup.contains("Go"),
        "{popup}"
    );
    let rows = app.commands.as_ref().map_or(0, Commands::len);
    key(&mut app, 'k');
    let last = app.commands.as_ref().and_then(|c| c.entry(c.cursor()));
    assert_eq!(
        (app.commands.as_ref().map(Commands::cursor), last),
        (Some(rows - 1), Some(Act::OpenTerminal))
    );
}

/// With nothing in particular to act on, the quick popup has View and Go,
/// which own the settings and places Here lists then. `space` or `tab`
/// expands it into the full panel on Here, `/` into the full panel's
/// search; `?` lists the keys instead, and `esc` closes it.
#[test]
fn space_again_expands_the_quick_popup_into_the_panel() {
    let mut phone = app(50, 30);
    let mut app = app(140, 40);
    key(&mut app, ' ');
    assert!(quick(&app));
    let popup = boxed(&mut app, "commands");
    assert!(!popup.contains("Here"), "nothing of Here's left:\n{popup}");
    insta::assert_snapshot!("commands_quick", screen(&mut app));
    key(&mut phone, ' ');
    insta::assert_snapshot!("commands_quick_narrow", screen(&mut phone));
    key(&mut app, ' ');
    assert_eq!((quick(&app), tab(&app)), (false, "Here"));
    let here = panel(&mut app);
    assert!(
        here.contains(" Here  View  Files  Review  Git  Copy "),
        "{here}"
    );
    esc(&mut app);
    assert!(app.commands.is_none());
    key(&mut app, ' ');
    press(&mut app, Key::Tab);
    assert_eq!((quick(&app), tab(&app)), (false, "Here"));
    esc(&mut app);
    key(&mut app, ' ');
    esc(&mut app);
    assert!(app.commands.is_none(), "esc closes the popup");
    typed(&mut app, " ?");
    assert!(app.help.is_some() && app.commands.is_none());
    esc(&mut app);
    key(&mut app, ' ');
    typed(&mut app, "/wrp");
    assert!(!quick(&app));
    let shown = screen(&mut app);
    assert!(
        shown.contains("/wrp") && shown.contains("Wrap long lines"),
        "{shown}"
    );
}

/// Here's keys come before the popup's other groups, which leave out a
/// row whose key a Here row takes: in a conflict once a choice is made,
/// `w` writes the file rather than toggling watching.
#[test]
fn heres_keys_win_over_the_quick_popups_other_groups() {
    let mut app = conflicted_app();
    key(&mut app, ' ');
    key(&mut app, 'j');
    press(&mut app, Key::Enter);
    key(&mut app, ' ');
    let popup = boxed(&mut app, "commands");
    assert!(popup.contains("In this conflict"), "{popup}");
    assert!(popup.contains("Write resolved file…"), "{popup}");
    assert!(!popup.contains("Watch for changes"), "{popup}");
    let done = press(&mut app, Key::Char('w'));
    assert!(
        matches!(done, Some(Action::WriteResolution { .. })),
        "{done:?}"
    );
    assert_eq!(app.watch, Watch::Off);
}

/// `space` twice shows the full panel on Here: with nothing in particular
/// to act on, a few settings and actions, and all keys after a blank.
/// The tabs head it, the open one highlighted; `tab` and `shift+tab` go
/// round them. A tab's rows have their state, their key and, dimmed,
/// the direct key when that's another, a tab of several groups titling
/// each; a tab goes in two columns only on a screen too short for one.
#[test]
fn the_command_panel_opens_on_here_and_tab_switches_tabs() {
    let mut phone = app(50, 30);
    let mut short = app(140, 14);
    let mut app = app(140, 40);
    typed(&mut app, "  ");
    assert_eq!(tab(&app), "Here");
    let here = panel(&mut app);
    assert!(
        here.contains(" Here  View  Files  Review  Git  Copy "),
        "{here}"
    );
    for label in [
        "Find file…",
        "Layout ‹",
        "Wrap long lines",
        "Watch for changes",
        "Open terminal here",
        "All keys",
    ] {
        assert!(here.contains(label), "{label}:\n{here}");
    }
    press(&mut app, Key::Tab);
    assert_eq!(tab(&app), "View");
    let view = panel(&mut app);
    for title in ["│ Layout ", "│ Diff ", " Display "] {
        assert!(view.contains(title), "{title}:\n{view}");
    }
    insta::assert_snapshot!("commands_wide", panel_markup(&mut app));
    press(&mut app, Key::BackTab);
    press(&mut app, Key::BackTab);
    assert_eq!(tab(&app), "Copy");
    insta::assert_snapshot!("commands_copy_tab", panel(&mut app));
    press(&mut app, Key::BackTab);
    assert_eq!(tab(&app), "Git");
    press(&mut app, Key::BackTab);
    insta::assert_snapshot!("commands_review_tab", panel(&mut app));
    let (x, y) = cell_of(&mut app, " View ");
    app.event(Event::Input(Input::Click { x: x + 1, y }));
    assert_eq!(tab(&app), "View", "a tap on its title");
    typed(&mut short, "  ");
    press(&mut short, Key::Tab);
    insta::assert_snapshot!("commands_short", screen(&mut short));
    typed(&mut phone, "  ");
    press(&mut phone, Key::Tab);
    insta::assert_snapshot!("commands_narrow", screen(&mut phone));
}

/// What can't apply isn't listed: staging off uncommitted changes, and
/// the review's entries off a review; their keys still run, and say why.
#[test]
fn entries_that_cant_apply_are_hidden_and_their_keys_say_why() {
    let mut app = app(140, 40);
    typed(&mut app, "  ");
    press(&mut app, Key::BackTab);
    assert!(panel(&mut app).contains("Copy selection"));
    press(&mut app, Key::BackTab);
    let git = panel(&mut app);
    assert!(
        !git.contains("Stage lines") && !git.contains("Since checkpoint"),
        "{git}"
    );
    assert!(git.contains("Take checkpoint"), "{git}");
    press(&mut app, Key::BackTab);
    let review = panel(&mut app);
    for hidden in ["Since last review", "Show whole review", "Open review…"] {
        assert!(!review.contains(hidden), "{hidden}:\n{review}");
    }
    assert!(review.contains("Delete resolved threads…"), "{review}");
    key(&mut app, 'A');
    assert!(status(&mut app).contains("staging works on uncommitted changes"));
    app.set_uncommitted(Some(Uncommitted::Plain));
    typed(&mut app, "  ");
    press(&mut app, Key::BackTab);
    press(&mut app, Key::BackTab);
    assert!(panel(&mut app).contains("Stage lines"));
}

/// On a focused thread, Here has what its keys do to it, each with the
/// key there when the panel has none of its own; `enter` on one runs it
/// as its key does.
#[test]
fn here_on_a_thread_lists_its_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (140, 40));
    for _ in 0..5 {
        if focused(&app) == Some("d1") {
            break;
        }
        key(&mut app, 'm');
    }
    assert_eq!(focused(&app), Some("d1"));
    typed(&mut app, "  ");
    insta::assert_snapshot!("commands_here_thread", panel(&mut app));
    let rows = app.commands.as_ref().map_or(0, Commands::len);
    let under = |app: &App| {
        let commands = app.commands.as_ref()?;
        commands.entry(commands.cursor())
    };
    for _ in 0..rows {
        if under(&app) == Some(Act::Reply) {
            break;
        }
        key(&mut app, 'j');
    }
    press(&mut app, Key::Enter);
    assert!(app.commands.is_none());
    assert!(matches!(app.composing, Some(Compose::Reply(ref root)) if root == "d1"));
}

/// On Here, a key a row shows runs that row, ahead of the leader's entry
/// on the same letter, and shows as a key, not dimmed: on a thread `r`
/// replies and `x` resolves. On another tab `r` is the rendered view's.
#[test]
fn heres_keys_run_its_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (140, 40));
    for _ in 0..5 {
        if focused(&app) == Some("d1") {
            break;
        }
        key(&mut app, 'm');
    }
    assert_eq!(focused(&app), Some("d1"));
    key(&mut app, ' ');
    let shown = panel_markup(&mut app);
    for (label, key) in [("Reply", 'r'), ("Resolve", 'x')] {
        let row = shown.lines().find(|l| l.contains(label));
        let dimmed = format!("<dim>{key}</>");
        assert!(
            row.is_some_and(|r| !r.contains(&dimmed)),
            "{label}:\n{shown}"
        );
    }
    key(&mut app, 'r');
    assert!(app.commands.is_none());
    assert!(matches!(app.composing, Some(Compose::Reply(ref root)) if root == "d1"));
    esc(&mut app);
    assert_eq!(focused(&app), Some("d1"));
    key(&mut app, ' ');
    key(&mut app, 'x');
    let resolved = (read_comments(&path).comments.iter()).any(|c| c.id == "d1" && c.resolved);
    assert!(resolved, "{}", status(&mut app));
    typed(&mut app, "  ");
    press(&mut app, Key::Tab);
    assert_eq!(tab(&app), "View");
    key(&mut app, 'r');
    assert!(app.composing.is_none());
    assert!(status(&mut app).contains("no renderer for .rs"));
}

/// A search finds Here's rows too, under Here's name, and runs them.
#[test]
fn a_search_finds_heres_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (140, 40));
    for _ in 0..5 {
        if focused(&app) == Some("d1") {
            break;
        }
        key(&mut app, 'm');
    }
    key(&mut app, ' ');
    typed(&mut app, "/reply");
    let found = panel(&mut app);
    let row = found.lines().find(|l| l.contains("Reply"));
    assert!(row.is_some_and(|r| r.contains("Here")), "{found}");
    press(&mut app, Key::Enter);
    press(&mut app, Key::Enter);
    assert!(matches!(app.composing, Some(Compose::Reply(ref root)) if root == "d1"));
}

/// With lines selected, Here has what the keys do to them, staging
/// included on uncommitted changes.
#[test]
fn here_with_a_selection_lists_what_the_keys_do_to_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(140, 40);
    app.set_uncommitted(Some(Uncommitted::Plain));
    screen(&mut app);
    typed(&mut app, "vj  ");
    insta::assert_snapshot!("commands_here_selection", panel(&mut app));
}

/// A file with a conflict whose markers say current and incoming.
fn conflicted_app() -> App {
    let mut fd = diff(FILE);
    let new = "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> topic\nz\n";
    fd.old = Text::new(b"a\nours\nz\n".to_vec());
    fd.new = Text::new(new.as_bytes().to_vec());
    fd.hunks = hunks(&fd.old, &fd.new, &Options::default());
    fd.change.status = Status::Unmerged;
    let conflicts = dffy_core::conflict::Conflicts {
        regions: dffy_core::conflict::regions(&fd.new, &dffy_core::conflict::Sides::default()),
        refused: None,
    };
    let loaded = File::prepare(crate::grammars(), None).conflicted(
        fd,
        conflicts,
        None,
        Whitespace::Exact,
        false,
    );
    let mut app = app_with(&Config::default(), "working tree", loaded, (140, 40));
    screen(&mut app);
    app
}

/// In a conflict on screen, Here has its choices with the conflict's
/// keys, and once one is made, taking it back and writing the file.
#[test]
fn here_in_a_conflict_lists_its_choices() {
    let mut app = conflicted_app();
    typed(&mut app, "  ");
    insta::assert_snapshot!("commands_here_conflict", panel(&mut app));
    key(&mut app, 'j');
    press(&mut app, Key::Enter);
    assert!(
        status(&mut app).contains("incoming taken"),
        "{}",
        status(&mut app)
    );
    typed(&mut app, "  ");
    let here = panel(&mut app);
    assert!(
        here.contains("Undo: Take incoming (theirs)") && here.contains("Write resolved file…"),
        "{here}"
    );
}

/// On a review, Here has its actions: posting and pulling by their keys
/// outside the panel, and "Since last review" by the panel's.
#[test]
fn here_on_a_review_lists_its_actions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (140, 40));
    typed(&mut app, "  ");
    let here = panel(&mut app);
    for row in ["Post review…", "Pull updates", "Since last review"] {
        assert!(here.contains(row), "{row}:\n{here}");
    }
    assert!(!here.contains("Show whole review"), "{here}");
}

/// A row's key runs it at once; a toggle keeps the panel open with its
/// box flipped, somewhere to go (the picker) closes it. `esc` closes.
#[test]
fn a_rows_key_runs_it_and_toggles_keep_the_panel_open() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    key(&mut app, 'W');
    assert!(app.wrap);
    assert!(app.commands.is_some(), "a toggle keeps it open");
    let shown = screen(&mut app);
    assert!(shown.contains("[x] Wrap long lines"), "{shown}");
    assert!(shown.lines().last().is_some_and(|l| l.contains("wrap on")));
    key(&mut app, 'o');
    assert!(app.commands.is_none(), "the picker closes it");
    assert!(app.picker.is_some());
    esc(&mut app);
    key(&mut app, ' ');
    key(&mut app, 'z');
    assert!(app.commands.is_some(), "a key with no entry keeps it open");
    assert!(status(&mut app).contains("no command"));
    esc(&mut app);
    assert!(app.commands.is_none());
}

/// Browsing: `j`/`k` move a cursor that goes round at the ends; `enter`
/// runs the row under it; `left`/`h` step a setting back, `right`/`l`
/// forward, and so does a tap on its `‹` or `›`.
#[test]
fn browsing_moves_a_cursor_and_enter_runs_its_row() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    let entries = app.commands.as_ref().map_or(0, Commands::len);
    let cursor = |app: &App| app.commands.as_ref().map(Commands::cursor);
    let under = |app: &App| {
        let commands = app.commands.as_ref()?;
        commands.entry(commands.cursor())
    };
    // Up from the first row is the last.
    key(&mut app, 'k');
    assert_eq!(cursor(&app), Some(entries - 1));
    key(&mut app, 'j');
    assert_eq!(cursor(&app), Some(0));
    // To the wrap row, then enter.
    for _ in 0..entries {
        if under(&app) == Some(Act::ToggleWrap) {
            break;
        }
        key(&mut app, 'j');
    }
    assert_eq!(under(&app), Some(Act::ToggleWrap));
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert!(app.wrap);
    assert!(app.commands.is_some());
    assert!(screen(&mut app).contains("[x] Wrap long lines"));
    // The layout: auto to split by its key, which puts the cursor on
    // it, then back to auto.
    key(&mut app, 's');
    assert_eq!(app.view, View::Split);
    assert_eq!(under(&app), Some(Act::CycleLayout));
    app.event(Event::Input(Input::Key(Key::Left, Mods::NONE)));
    assert_eq!(app.view, View::Auto);
    key(&mut app, 'h');
    assert_eq!(app.view, View::Unified);
    key(&mut app, 'l');
    assert_eq!(app.view, View::Auto);
    let shown = screen(&mut app);
    let (y, row) = shown
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("Layout ‹"))
        .expect("the layout row");
    let left = row.chars().position(|c| c == '‹').expect("‹");
    let right = row.chars().position(|c| c == '›').expect("›");
    let tap = |app: &mut App, x: usize| {
        app.event(Event::Input(Input::Click {
            x: u16::try_from(x).expect("column"),
            y: u16::try_from(y).expect("row"),
        }));
    };
    tap(&mut app, left);
    assert_eq!(app.view, View::Unified);
    tap(&mut app, right);
    assert_eq!(app.view, View::Auto);
}

/// In the panel `h`/`l` only step values: off a value row they do
/// nothing. Highlighting and line numbers are on `*` and `#`, so they
/// work from any row, the layout row `s` leaves the cursor on included,
/// and from any tab: a setting's key shows the tab it's in.
#[test]
fn h_and_l_only_step_values_in_the_panel() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    let (highlight, numbers) = (app.highlight, app.line_numbers);
    key(&mut app, 'h');
    key(&mut app, 'l');
    assert_eq!(
        (app.highlight, app.line_numbers, app.view),
        (highlight, numbers, View::Auto)
    );
    assert!(!status(&mut app).contains("no command"));
    key(&mut app, 's');
    assert_eq!(tab(&app), "Here");
    key(&mut app, '*');
    assert_eq!(tab(&app), "View");
    key(&mut app, '#');
    assert_eq!(app.view, View::Split);
    assert_eq!((app.highlight, app.line_numbers), (!highlight, !numbers));
    assert!(app.commands.is_some());
}

/// The theme row opens a list of every theme, grouped; the cursor
/// tries each theme on the screen behind, `esc` brings back the one it
/// opened on, `enter` keeps the one under the cursor and remembers it.
#[test]
fn the_theme_list_tries_themes_live_and_esc_restores() {
    let mut app = app(140, 40);
    // The built-ins only, whatever the machine's theme directory has.
    app.themes = theme::builtin_names()
        .map(|name| {
            let source = theme::dump(name).expect("built-in");
            (
                name.to_owned(),
                Theme::parse(name, &source, &mut Vec::new()),
            )
        })
        .collect();
    key(&mut app, ' ');
    let names = app
        .themes
        .iter()
        .map(|(n, t)| (n.clone(), t.group()))
        .collect();
    app.commands
        .as_mut()
        .expect("open")
        .open_themes(names, "default");
    // The list alone: what's around it is the panel's snapshot.
    let shown = screen(&mut app);
    let lines: Vec<Vec<char>> = shown.lines().map(|l| l.chars().collect()).collect();
    let top = lines
        .iter()
        .position(|l| l.iter().collect::<String>().contains("─ themes ─"))
        .expect("the list");
    let left = lines[top].iter().position(|&c| c == '┌').expect("corner");
    let right = left
        + lines[top][left..]
            .iter()
            .position(|&c| c == '┐')
            .expect("corner");
    let list: Vec<String> = lines[top..]
        .iter()
        .take_while(|l| l.get(left) != Some(&'└'))
        .map(|l| l[left..=right].iter().collect())
        .collect();
    insta::assert_snapshot!("theme_list", list.join("\n"));
    let down = |app: &mut App| app.event(Event::Input(Input::Key(Key::Down, Mods::NONE)));
    down(&mut app);
    down(&mut app);
    assert_eq!(app.theme_name, "github-dark");
    assert_eq!(
        app.theme.style(Role::Added).fg,
        Some(theme::Color::Rgb(0x3f, 0xb9, 0x50))
    );
    esc(&mut app);
    assert_eq!(app.theme_name, "default");
    assert_eq!(app.theme.style(Role::Added).fg, Some(theme::Color::Ansi(2)));
    assert!(
        app.commands.as_ref().is_some_and(|c| !c.themes_open()),
        "back to the panel"
    );
    let names = app
        .themes
        .iter()
        .map(|(n, t)| (n.clone(), t.group()))
        .collect();
    app.commands
        .as_mut()
        .expect("open")
        .open_themes(names, "default");
    for c in "latte".chars() {
        key(&mut app, c);
    }
    assert_eq!(app.theme_name, "catppuccin-latte");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert_eq!(app.theme_name, "catppuccin-latte");
    assert!(
        app.remembered
            .iter()
            .any(|(k, v)| *k == "theme" && v == "catppuccin-latte")
    );
    assert!(app.commands.as_ref().is_some_and(|c| !c.themes_open()));
    // The row's key steps forward and puts the cursor on it; there,
    // left steps back, without the list.
    key(&mut app, '~');
    assert_eq!(app.theme_name, "gruvbox-light");
    app.event(Event::Input(Input::Key(Key::Left, Mods::NONE)));
    assert_eq!(app.theme_name, "catppuccin-latte");
    assert!(app.commands.as_ref().is_some_and(|c| !c.themes_open()));
}

/// `/` in the panel narrows it with the fuzzy matcher, marking what
/// matched; `esc` clears the filter, then closes.
#[test]
fn the_panel_filters_fuzzily() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    for c in "/wrp".chars() {
        key(&mut app, c);
    }
    insta::assert_snapshot!("commands_filtered", screen(&mut app));
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    esc(&mut app);
    assert!(app.commands.is_some(), "esc clears the filter first");
    esc(&mut app);
    assert!(app.commands.is_none());
}

/// A tap runs the row under it, in either column.
#[test]
fn a_tap_on_a_row_runs_it() {
    let mut app = app(140, 40);
    typed(&mut app, "  ");
    press(&mut app, Key::Tab);
    let (x, y) = cell_of(&mut app, "Wrap long lines");
    app.event(Event::Input(Input::Click { x, y }));
    assert!(app.wrap);
    let headers = app.hunk_headers;
    let (x, y) = cell_of(&mut app, "Hunk headers");
    app.event(Event::Input(Input::Click { x, y }));
    assert_ne!(app.hunk_headers, headers, "the right column's row");
}

/// The panel's copy copies the selection made before it opened.
#[test]
fn the_panels_copy_copies_the_selection() {
    let mut app = app(140, 40);
    app.draw();
    for c in "vj ".chars() {
        key(&mut app, c);
    }
    assert!(app.commands.is_some());
    let copied = app.event(Event::Input(Input::Key(Key::Char('y'), Mods::NONE)));
    assert_eq!(
        copied,
        Some(Action::Copy {
            text: b"line 1\nline 2".to_vec(),
            note: "copied 2 lines".to_owned(),
        })
    );
}

/// Highlighting or whitespace toggled from the panel diffs every file
/// again with the new settings.
#[test]
fn toggling_highlighting_diffs_again() {
    let mut app = app(140, 40);
    app.draw();
    let version = app.version(0);
    key(&mut app, ' ');
    let done = app.event(Event::Input(Input::Key(Key::Char('*'), Mods::NONE)));
    assert_eq!(
        done,
        Some(Action::Rediff {
            highlight: false,
            whitespace: Whitespace::Exact
        })
    );
    assert_ne!(app.version(0), version, "work asked for before is void");
    assert!(
        app.doc.document.files()[0].wants_diff(),
        "the shown file is diffed again"
    );
    let done = app.event(Event::Input(Input::Key(Key::Char('I'), Mods::NONE)));
    assert_eq!(
        done,
        Some(Action::Rediff {
            highlight: false,
            whitespace: Whitespace::IgnoreAll
        })
    );
}

/// Expanding the quick popup leaves nothing of it on the terminal: the
/// frames sent, played in an emulator, show the full panel over the diff
/// alone, after the popup stood through several frames.
#[test]
fn expanding_the_quick_popup_leaves_none_of_it_on_screen() {
    let (width, height) = (160, 40);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir, (width, height));
    let size =
        |n: usize| std::num::NonZero::new(u16::try_from(n).expect("fits")).expect("non-zero");
    let mut vt = vt100::Parser::new(size(height), size(width), 0);
    app.invalidate();
    let mut frame = |app: &mut App| {
        app.dirty = true;
        vt.process(app.draw().unwrap_or_default());
        if place(app) {
            vt.process(app.draw().unwrap_or_default());
        }
        vt.screen().contents()
    };
    frame(&mut app);
    key(&mut app, ' ');
    let popup = frame(&mut app);
    assert!(popup.contains("space more"), "{popup}");
    key(&mut app, 'j');
    frame(&mut app);
    key(&mut app, 'W');
    frame(&mut app);
    key(&mut app, ' ');
    assert!(!quick(&app));
    for _ in 0..2 {
        let shown = frame(&mut app);
        for gone in ["space more", "On this thread", "Watch for changes"] {
            assert!(!shown.contains(gone), "{gone}:\n{shown}");
        }
        let lines = |text: &str| -> Vec<String> {
            (text.trim_end().lines())
                .map(|l| l.trim_end().to_owned())
                .collect()
        };
        assert_eq!(lines(&shown), lines(&app.screen.text()));
    }
}
