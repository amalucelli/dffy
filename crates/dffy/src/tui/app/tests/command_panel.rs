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

/// `space` opens the command panel: groups, a row per entry with its
/// state, its key and, dimmed, the direct key when that's another.
#[test]
fn the_command_panel_lists_entries_with_states_and_keys() {
    let mut phone = app(50, 30);
    let mut app = app(140, 40);
    key(&mut app, ' ');
    assert!(app.commands.is_some());
    insta::assert_snapshot!("commands_wide", panel_markup(&mut app));
    key(&mut phone, ' ');
    insta::assert_snapshot!("commands_narrow", screen(&mut phone));
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
    let entries = app.command_entries().len();
    let cursor = |app: &App| app.commands.as_ref().map(Commands::cursor);
    let under = |app: &App| {
        let commands = app.commands.as_ref()?;
        commands.entry(&app.command_entries(), commands.cursor())
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
/// work from any row, the layout row `s` leaves the cursor on included.
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
    key(&mut app, '*');
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
    key(&mut app, 'a');
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

#[test]
fn a_tap_on_a_row_runs_it() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    let shown = screen(&mut app);
    let y = shown
        .lines()
        .position(|l| l.contains("Wrap long lines"))
        .expect("the wrap row");
    app.event(Event::Input(Input::Click {
        x: 70,
        y: u16::try_from(y).expect("row"),
    }));
    assert!(app.wrap);
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
