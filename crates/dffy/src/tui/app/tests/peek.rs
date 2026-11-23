//! Go to definition in the viewer, its jobs run here as the pool would run
//! them: answers from an index being built, refreshes, failures and what
//! the keys and the mouse do to the peek.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use dffy_core::git::{DiffSpec, Revisions};

use super::support::*;
use crate::navigator::client::{Built, Pushed};
use crate::tui::peek::Found;
use crate::tui::worker::{self, Job, Settings, Unanswered};

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?}");
}

const DEFINES: &str = "static inline void *kmalloc_array(unsigned long n, unsigned long s)\n\
                       {\n\treturn 0;\n}\n";

/// A C repository whose `mm/use.c` calls `kmalloc_array`, which
/// `include/slab.h` (included), `mm/a.c`, `mm/x.c` and
/// `tools/include/slab.h` define. The second commit changes `mm/use.c`,
/// `mm/x.c` and `README.md`; with `uncommitted`, those changes are left
/// in the working tree instead.
struct Fixture {
    dir: tempfile::TempDir,
    _cache: tempfile::TempDir,
    repo: Repo,
    spec: DiffSpec,
    settings: Settings,
    app: App,
    /// What the helper pushes of its index, as the pool's events carry it.
    pushed: (mpsc::Sender<Event>, mpsc::Receiver<Event>),
}

impl Fixture {
    fn new(width: usize, uncommitted: bool) -> Self {
        crate::navigator::client::use_program(crate::navigator::built_program());
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        for folder in ["include", "mm", "tools/include"] {
            std::fs::create_dir_all(d.join(folder)).expect("mkdir");
        }
        let user = |size: &str| {
            format!(
                "#include <slab.h>\n\nint use(void)\n{{\n\tvoid *p = kmalloc_array(4, {size});\n\treturn p != 0;\n}}\n"
            )
        };
        let write = |path: &str, text: &str| std::fs::write(d.join(path), text).expect("write");
        write("include/slab.h", DEFINES);
        write("tools/include/slab.h", DEFINES);
        write("mm/a.c", DEFINES);
        write("mm/x.c", "int x;\n");
        write("mm/use.c", &user("8"));
        write("README.md", "# notes\n");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-q", "-m", "first"]);
        write("mm/use.c", &user("16"));
        write("mm/x.c", &format!("int x;\n{DEFINES}"));
        write("README.md", "# notes, kmalloc_array\n");
        if !uncommitted {
            git(d, &["commit", "-q", "-am", "second"]);
        }
        let repo = Repo::discover(d).expect("repo");
        let revs: Vec<&str> = if uncommitted {
            vec![]
        } else {
            vec!["HEAD~1", "HEAD"]
        };
        let spec = repo.resolve(&revs, false, false).expect("resolves");
        let cache = tempfile::tempdir().expect("tempdir");
        let settings = Settings {
            options: Options::default(),
            highlight: false,
            grammars: crate::grammars(),
            cache: Some(dffy_core::cache::Cache::new(
                cache.path().to_owned(),
                1 << 24,
            )),
        };
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "test".to_owned(),
            None,
            (width, 40),
            Indicators::Classic,
        );
        app.focus("mm/use.c".into());
        repo.for_each_change(&spec, &[], |change| {
            app.event(Event::Enumerated(change));
            ControlFlow::Continue(())
        })
        .expect("enumerates");
        app.event(Event::EnumerationDone(None));
        app.set_snapshots(spec.snapshots(false));
        let mut fixture = Self {
            dir,
            _cache: cache,
            repo,
            spec,
            settings,
            app,
            pushed: mpsc::channel(),
        };
        fixture.run(|_| true);
        fixture
    }

    /// Draws and runs the jobs `which` keeps until none is asked for, then
    /// takes what the helper pushes until its index is built.
    fn run(&mut self, which: impl Fn(&Job) -> bool) {
        self.run_jobs(which, true);
    }

    /// Draws and runs the jobs `which` keeps until none is asked for,
    /// taking what the helper pushed meanwhile; with `built`, waits for
    /// pushes until the index of the version asked about is built.
    fn run_jobs(&mut self, which: impl Fn(&Job) -> bool, built: bool) {
        for _ in 0..10_000 {
            self.app.draw();
            let jobs: Vec<Job> = self.app.jobs().into_iter().filter(|j| which(j)).collect();
            if jobs.is_empty() {
                if let Ok(pushed) = self.pushed.1.try_recv() {
                    self.app.event(pushed);
                    continue;
                }
                if built && self.app.peek_building() {
                    let pushed = (self.pushed.1)
                        .recv_timeout(Duration::from_secs(30))
                        .expect("the helper pushes how far its index is");
                    self.app.event(pushed);
                    continue;
                }
                return;
            }
            for job in jobs {
                let done = worker::run_pushing(
                    job,
                    Some(&self.repo),
                    &self.settings,
                    Some(&self.pushed.0),
                );
                self.app.event(Event::Diffed(done));
            }
        }
        panic!("the jobs never ran out");
    }

    /// Everything asked, the helper's pushes taken as they came.
    fn run_asked(&mut self) {
        self.run_jobs(|_| true, false);
    }

    /// Peeks at `kmalloc_array` from its call in `mm/use.c`.
    fn peek(&mut self) {
        assert!(
            self.app.peek_name("kmalloc_array", document::Side::New, 4),
            "asks"
        );
    }

    fn found(&self) -> Option<(usize, Option<String>, String)> {
        let peek = self.app.doc.def_peek()?;
        let Found::Listed(looked) = peek.found() else {
            return None;
        };
        let current = peek
            .current()
            .map(|c| c.path.to_string())
            .unwrap_or_default();
        Some((looked.total, peek.note.clone(), current))
    }

    fn key(&mut self, key: Key) {
        self.app.event(Event::Input(Input::Key(key, Mods::NONE)));
    }

    fn message(&self) -> String {
        (self.app.message.as_ref()).map_or_else(String::new, |(m, _)| m.clone())
    }

    /// The screen cell `text` starts at, the first time it shows.
    fn cell_of(&mut self, text: &str) -> (u16, u16) {
        let screen = screen(&mut self.app);
        let (y, line) = (screen.lines().enumerate())
            .find(|(_, l)| l.contains(text))
            .unwrap_or_else(|| panic!("{text} isn't on screen:\n{screen}"));
        let x = line[..line.find(text).unwrap_or(0)].chars().count();
        (
            u16::try_from(x).expect("fits"),
            u16::try_from(y).expect("fits"),
        )
    }
}

/// Asked while the index is being built, the peek lists what's found so
/// far and says how far that is. Once the index is complete it asks again
/// on its own, the cursor kept on the definition it was on.
#[test]
fn a_partial_answer_is_asked_again_once_indexed_with_the_cursor_kept() {
    let mut f = Fixture::new(160, false);
    f.peek();
    f.run_jobs(|j| matches!(j, Job::Navigator(_)), true);
    // The helper builds so small an index before any lookup reaches it:
    // the answer is made one from an index half built.
    let (generation, navigator) = f.app.peek_navigator(document::Side::New).expect("opened");
    f.app.draw();
    let lookup = (f.app.jobs().into_iter())
        .find(|j| matches!(j, Job::Lookup(_)))
        .expect("a lookup asked for");
    let Done::Looked {
        id,
        result: Ok(mut looked),
    } = worker::run(lookup, Some(&f.repo), &f.settings)
    else {
        panic!("the lookup answers");
    };
    looked
        .candidates
        .retain(|c| c.path == "include/slab.h" || c.path == "mm/x.c");
    looked.total = looked.candidates.len();
    let whole = looked.progress;
    looked.progress.done = 1;
    f.app.event(Event::Diffed(Done::Looked {
        id,
        result: Ok(looked),
    }));
    f.run_asked();
    let (total, note, current) = f.found().expect("listed");
    assert_eq!(
        total, 2,
        "the included header and the changed file next door"
    );
    assert!(note.is_some_and(|n| n.starts_with("partial: ")));
    assert_eq!(current, "include/slab.h");
    f.key(Key::Char('j'));
    assert_eq!(f.found().expect("listed").2, "mm/x.c");
    let push = |progress| Event::IndexBuilt {
        generation,
        pushed: Pushed {
            navigator,
            built: Built {
                progress,
                building: !progress.complete(),
                ..Built::default()
            },
        },
    };
    let half = dffy_core::navigate::Progress { done: 1, ..whole };
    f.app.event(push(half));
    f.app.dirty = false;
    f.app.event(push(half));
    assert!(!f.app.dirty, "the same progress draws nothing");
    // The helper pushes that its index is complete.
    f.app.event(push(whole));
    f.run(|_| true);
    let (total, note, current) = f.found().expect("listed");
    assert_eq!((total, note), (4, None), "the whole index's answer");
    assert_eq!(current, "mm/x.c", "the cursor stayed on its definition");
}

/// A refresh brings the version the peek shows up to date and asks again:
/// the working tree's new lines show.
#[test]
fn a_refresh_reads_the_shown_version_again() {
    let mut f = Fixture::new(160, true);
    f.peek();
    f.run(|_| true);
    while f.found().is_some_and(|(.., current)| current != "mm/x.c") {
        f.key(Key::Char('j'));
    }
    f.run(|_| true);
    let code = |f: &mut Fixture| screen(&mut f.app);
    assert!(code(&mut f).contains("return 0;"));
    std::fs::write(
        f.dir.path().join("mm/x.c"),
        format!("int x;\n{}", DEFINES.replace("return 0;", "return grown;")),
    )
    .expect("write");
    let mut set = ChangeSet::new(f.repo.clone(), f.spec, Revisions::default(), &[], false);
    set.enumerate(|_| ControlFlow::Continue(()))
        .expect("enumerates");
    let update = set
        .refresh(&["mm/x.c".into()])
        .expect("refreshes")
        .expect("a change");
    f.app.event(Event::Refreshed(update));
    f.run(|_| true);
    assert!(code(&mut f).contains("return grown;"), "{}", code(&mut f));
    assert!(f.found().is_some(), "the peek stayed");
}

/// A navigator that can't be made, and a tags query that doesn't compile,
/// are said: the first in the peek, the second in the status bar.
#[test]
fn failures_are_said() {
    let mut f = Fixture::new(160, false);
    f.peek();
    let id = (f.app.jobs().into_iter())
        .find_map(|j| match j {
            Job::Navigator(job) => Some(job.id),
            _ => None,
        })
        .expect("a navigator asked for");
    f.app.event(Event::Diffed(Done::Navigator {
        side: document::Side::New,
        id,
        result: Err(Unanswered::Failed("boom".to_owned())),
    }));
    // The version goes by the description here: nothing named its revisions.
    assert!(screen(&mut f.app).contains("couldn't list the files in test: boom"));

    f.peek();
    f.run_jobs(|j| matches!(j, Job::Navigator(_)), true);
    let (generation, navigator) = f.app.peek_navigator(document::Side::New).expect("opened");
    let bad = || Event::IndexBuilt {
        generation,
        pushed: Pushed {
            navigator,
            built: Built {
                error: Some("bad query".to_owned()),
                ..Built::default()
            },
        },
    };
    f.app.event(bad());
    assert_eq!(f.message(), "go to definition: bad query");
    f.app.message = None;
    f.app.dirty = false;
    f.app.event(bad());
    assert_eq!(f.message(), "", "said once");
    assert!(!f.app.dirty, "the same push draws nothing");
}

/// The helper killed while the viewer runs: the next ask starts another,
/// whose ids never meet the dead one's, the other version's navigator and
/// the search for uses of the dead one started again with it, and after
/// three starts within a minute the peek says the helper keeps exiting.
#[test]
fn a_helper_that_dies_is_started_again_a_few_times() {
    let mut f = Fixture::new(160, false);
    let kill = |f: &Fixture| {
        let pid = f
            .app
            .peek_helper()
            .and_then(|h| h.pid())
            .expect("a helper runs");
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).ok();
        // Its client sees it go before the next ask.
        let start = Instant::now();
        while f.app.peek_helper().is_some_and(|h| h.pid().is_some()) {
            assert!(start.elapsed() < Duration::from_secs(10), "it never went");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let both = |f: &mut Fixture| {
        f.peek();
        f.run(|_| true);
        let new = f.found().expect("listed on the new side");
        f.key(Key::Char('s'));
        f.run(|_| true);
        let old = f.found().expect("listed on the old side");
        (new, old)
    };
    let before = both(&mut f);
    let first = [document::Side::New, document::Side::Old].map(|s| f.app.peek_navigator(s));
    f.key(Key::Esc);
    f.peek();
    f.run(|_| true);
    f.key(Key::Char('r'));
    let searched = f.app.peek_search_generation();
    assert_eq!(searched, first[0].map(|(g, _)| g), "a search for uses");
    kill(&f);
    f.run(|_| true);
    let now = f.app.peek_navigator(document::Side::New).map(|(g, _)| g);
    assert_ne!(now, searched, "another helper");
    let shown = screen(&mut f.app);
    assert!(
        shown.contains("kmalloc_array · 2 files use it"),
        "the search started again: {shown}"
    );
    f.key(Key::Esc);
    assert_eq!(both(&mut f), before, "the same answers from another helper");
    let second = [document::Side::New, document::Side::Old].map(|s| f.app.peek_navigator(s));
    for (a, b) in first.iter().zip(&second) {
        let (a, b) = (a.expect("opened"), b.expect("opened"));
        assert_ne!(a.0, b.0, "each navigator of the new helper's generation");
    }
    kill(&f);
    both(&mut f);
    kill(&f);
    f.peek();
    f.run(|_| true);
    let shown = screen(&mut f.app);
    assert!(
        shown.contains("definitions unavailable: the helper keeps exiting"),
        "{shown}"
    );
}

/// The peek's code is read right after the work on the files on screen,
/// ahead of the work off screen: the search's counts of the other files.
#[test]
fn the_peeks_code_is_read_before_work_off_screen() {
    let mut f = Fixture::new(160, false);
    for c in "/kmalloc".chars() {
        f.key(Key::Char(c));
    }
    f.peek();
    f.run(|j| !matches!(j, Job::PeekCode(_) | Job::Search { .. }));
    let jobs = f.app.jobs();
    let at = |wanted: fn(&Job) -> bool| jobs.iter().position(wanted);
    let code = at(|j| matches!(j, Job::PeekCode(_))).expect("the code asked for");
    let counts = at(|j| matches!(j, Job::Search { .. })).expect("counts asked for");
    assert!(code < counts, "{code} before {counts}");
}

/// A search for uses tagged with a helper gone's generation isn't stepped
/// in the running one; its navigator opened again there starts it over.
#[test]
fn a_search_of_a_helper_gone_is_started_over() {
    let mut f = Fixture::new(160, false);
    f.peek();
    f.run(|_| true);
    f.key(Key::Char('r'));
    let (generation, _) = f.app.peek_navigator(document::Side::New).expect("opened");
    f.app.tag_peek_search(generation - 1);
    assert!(
        !f.app.jobs().iter().any(|j| matches!(j, Job::References(_))),
        "asked of another helper"
    );
    f.app.reopen_peek_navigator(document::Side::New);
    f.run(|_| true);
    let shown = screen(&mut f.app);
    assert!(shown.contains("kmalloc_array · 2 files use it"), "{shown}");
}

/// A click on a candidate in the list shows it; on a narrow screen the
/// list goes above the code.
#[test]
fn a_click_picks_a_candidate_and_a_narrow_peek_stacks() {
    let mut f = Fixture::new(160, false);
    f.peek();
    f.run(|_| true);
    let (x, y) = f.cell_of("tools/include/slab.h");
    f.app.event(Event::Input(Input::Click { x, y }));
    assert_eq!(f.found().expect("listed").2, "tools/include/slab.h");

    let mut f = Fixture::new(80, false);
    f.peek();
    f.run(|_| true);
    let shown = screen(&mut f.app);
    let row = |text: &str| shown.lines().position(|l| l.contains(text));
    let (list, rule, code) = (row("include/slab.h"), row("├──"), row("return 0;"));
    assert!(list < rule && rule < code, "{shown}");
}

/// Where go to definition can't run, `K` says why.
#[test]
fn k_says_why_it_cant_look() {
    let mut f = Fixture::new(160, false);
    f.app
        .event(Event::Input(Input::Key(Key::Char('F'), Mods::NONE)));
    f.run(|_| true);
    f.key(Key::Char('K'));
    assert_eq!(
        f.message(),
        "go to definition doesn't read README.md's language"
    );
    f.app.set_disk_cache(false);
    f.key(Key::Char('K'));
    assert!(f.message().contains("disk cache"), "{}", f.message());
    f.app.set_disk_cache(true);
    f.app.set_piped();
    f.key(Key::Char('K'));
    assert_eq!(
        f.message(),
        "a piped diff has no repository to look names up in"
    );
}

/// The peek closing, or its file leaving the screen, stops what it waits
/// on; `esc` closes one that gave the keys back. A while after, with no
/// `K`, the navigators go.
#[test]
fn a_peek_gone_stops_its_work_and_idle_navigators_go() {
    let mut f = Fixture::new(160, false);
    f.peek();
    f.key(Key::Char('f'));
    assert!(f.app.doc.def_peek().is_none());
    assert!(
        !f.app.jobs().iter().any(|j| matches!(j, Job::Lookup(_))),
        "a lookup for a peek gone"
    );

    f.key(Key::Char('F'));
    f.run(|_| true);
    f.peek();
    f.run(|_| true);
    let (x, y) = f.cell_of("int use(void)");
    f.app.event(Event::Input(Input::Click { x, y }));
    f.key(Key::Esc);
    assert!(
        f.app.doc.def_peek().is_none(),
        "esc closes a peek without the keys"
    );

    let deadline = f.app.navigation_deadline().expect("a deadline");
    assert!(deadline > Instant::now() + Duration::from_secs(9 * 60));
    f.app.navigation.used = Instant::now().checked_sub(Duration::from_secs(11 * 60));
    f.app.event(Event::Tick);
    assert!(f.app.navigation_deadline().is_none(), "dropped");
    assert!(f.app.peek_helper().is_none(), "the helper closed");
}

/// A ctrl+click away from the diff's code is a plain click.
#[test]
fn a_ctrl_click_off_code_is_a_click() {
    let mut f = Fixture::new(160, false);
    let (x, y) = f.cell_of("x.c");
    f.app.event(Event::Input(Input::PeekClick { x, y }));
    assert!(f.app.doc.def_peek().is_none());
    assert_eq!(
        f.app.doc.document.files()[f.app.doc.document.pos.file]
            .change()
            .path,
        "mm/x.c",
        "the tree opened it"
    );
}

/// `r` in the peek lists the lines using the name, as they're found, past
/// its definitions; closing the peek stops the search, and the answer is
/// kept for the next ask. `enter` opens a use as it opens a definition.
#[test]
fn r_lists_the_uses_and_keeps_them() {
    let mut f = Fixture::new(160, false);
    f.peek();
    f.run(|_| true);
    f.key(Key::Char('r'));
    assert!(
        f.app.jobs().iter().any(|j| matches!(j, Job::References(_))),
        "a search asked for"
    );
    f.run(|_| true);
    let shown = screen(&mut f.app);
    assert!(shown.contains("kmalloc_array · 2 files use it"), "{shown}");
    let peek = f.app.doc.def_peek().expect("open");
    let uses: Vec<String> = (peek.references().expect("searched").hits.iter())
        .map(|c| format!("{}:{}", c.path, c.start + 1))
        .collect();
    assert_eq!(
        uses,
        ["mm/use.c:5", "README.md:1"],
        "the definitions left out"
    );

    f.key(Key::Char('r'));
    assert!(
        screen(&mut f.app).contains("4 definitions"),
        "r again: the definitions"
    );
    f.key(Key::Esc);
    f.peek();
    f.run(|_| true);
    f.key(Key::Char('r'));
    assert!(
        !f.app.jobs().iter().any(|j| matches!(j, Job::References(_))),
        "kept from before"
    );
    assert!(screen(&mut f.app).contains("2 files use it"));
    f.key(Key::Enter);
    f.run(|_| true);
    assert!(f.app.doc.def_peek().is_none());
    assert_eq!(
        f.app.doc.document.files()[f.app.doc.document.pos.file]
            .change()
            .path,
        "mm/use.c"
    );

    f.peek();
    f.run(|_| true);
    f.app.navigation.forget_references();
    f.key(Key::Char('r'));
    f.key(Key::Esc);
    assert!(
        !f.app.jobs().iter().any(|j| matches!(j, Job::References(_))),
        "closing the peek stops the search"
    );
}

/// A refresh that moves the version a search for uses reads stops it and
/// clears its lines: they're of files no longer there. `r` again searches
/// the version as it is now.
#[test]
fn a_refresh_moving_the_searched_version_stops_the_search() {
    let mut f = Fixture::new(160, false);
    f.peek();
    f.run(|_| true);
    f.key(Key::Char('r'));
    assert!(
        f.app.jobs().iter().any(|j| matches!(j, Job::References(_))),
        "a search asked for"
    );
    let d = f.dir.path();
    std::fs::write(d.join("mm/z.c"), "void z(void) { kmalloc_array(1, 2); }\n").expect("write");
    // The same files change again, so the list keeps their rows.
    for path in ["mm/use.c", "mm/x.c", "README.md"] {
        let text = std::fs::read_to_string(d.join(path)).expect("read");
        std::fs::write(d.join(path), format!("{text}\n")).expect("write");
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "third"]);
    let revisions = Revisions {
        revs: vec!["HEAD~1".to_owned(), "HEAD".to_owned()],
        ..Revisions::default()
    };
    let mut set = ChangeSet::new(f.repo.clone(), f.spec, revisions, &[], false);
    set.enumerate(|_| ControlFlow::Continue(()))
        .expect("enumerates");
    let update = set.rescan().expect("rescans").expect("moved");
    f.app.set_snapshots(update.snapshots);
    f.app.event(Event::Refreshed(update));
    f.app.draw();
    assert!(
        !f.app.jobs().iter().any(|j| matches!(j, Job::References(_))),
        "the search of the version before stopped"
    );
    let peek = f.app.doc.def_peek().expect("open");
    assert!(!peek.on_references() && peek.references().is_none_or(|r| r.hits.is_empty()));

    f.run(|_| true);
    f.key(Key::Char('r'));
    f.run(|_| true);
    let peek = f.app.doc.def_peek().expect("open");
    let uses: Vec<String> = (peek.references().expect("searched").hits.iter())
        .map(|c| c.path.to_string())
        .collect();
    assert!(uses.contains(&"mm/z.c".to_owned()), "{uses:?}");
}

/// A refresh whose list is shorter leaves the viewer's work valid before
/// the next frame: the files the last frame showed may be gone.
#[test]
fn jobs_are_valid_after_a_refresh_shrinks_the_list() {
    let mut f = Fixture::new(160, false);
    f.peek();
    f.run(|_| true);
    f.key(Key::Char('r'));
    drop(f.app.jobs());
    let d = f.dir.path();
    std::fs::write(d.join("mm/z.c"), "void z(void) { kmalloc_array(1, 2); }\n").expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "third"]);
    let revisions = Revisions {
        revs: vec!["HEAD~1".to_owned(), "HEAD".to_owned()],
        ..Revisions::default()
    };
    let mut set = ChangeSet::new(f.repo.clone(), f.spec, revisions, &[], false);
    set.enumerate(|_| ControlFlow::Continue(()))
        .expect("enumerates");
    let update = set.rescan().expect("rescans").expect("moved");
    assert_eq!(update.files.len(), 1);
    f.app.set_snapshots(update.snapshots);
    f.app.event(Event::Refreshed(update));
    drop(f.app.jobs());
    f.run(|_| true);
}
