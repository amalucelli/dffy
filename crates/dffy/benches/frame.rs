//! TUI frame time at 200×60 against the 8ms budget, on the Linux range
//! v6.0..v6.1: its largest file (too large to highlight), `kernel/sched/fair.c`
//! with and without highlighting, whole and in file view, the file tree
//! over the whole range, the picker over it and v6.1's other files, and
//! go to definition's peek.
//! Needs the kernel clone named by `DFFY_BENCH_REPO`; skipped without it.
//! Timings are reported, never asserted.
#![expect(
    clippy::expect_used,
    reason = "benchmark setup: a failed step leaves nothing to measure"
)]

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::LazyLock;

use dffy::bench::{AllFiles, Frame, Peeking, Picking, Trees};
use dffy_core::cache::Cache;
use dffy_core::diff::Options;
use dffy_core::git::{FileChange, FileDiff, Repo};

// Allocation counts are deterministic, so `cargo xtask perf` gates on them
// with no noise band. Only the benchmark thread is counted.
#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const LARGEST: &str = "drivers/net/wireless/realtek/rtw89/rtw8852c_table.c";
const HIGHLIGHTED: &str = "kernel/sched/fair.c";
const SIZE: (usize, usize) = (200, 60);
/// Rows scrolled before measuring, so the screen is inside a large hunk.
const START: usize = 1000;

static CHANGES: LazyLock<Vec<FileChange>> = LazyLock::new(|| {
    let Some(root) = std::env::var_os("DFFY_BENCH_REPO") else {
        return Vec::new();
    };
    let repo = Repo::discover(&PathBuf::from(root)).expect("bench repo opens");
    let spec = repo
        .resolve(&["v6.0", "v6.1"], false, false)
        .expect("tags resolve");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    changes
});

static DIFFS: LazyLock<Option<(FileDiff, FileDiff)>> = LazyLock::new(|| {
    let root = PathBuf::from(std::env::var_os("DFFY_BENCH_REPO")?);
    let repo = Repo::discover(&root).expect("bench repo opens");
    let find = |path: &str| CHANGES.iter().find(|c| c.path == path).cloned();
    let diff = |change: Option<_>| {
        repo.file_diff(
            change.expect("file changed in the range"),
            &Options::default(),
        )
        .expect("diffs")
    };
    Some((diff(find(LARGEST)), diff(find(HIGHLIGHTED))))
});

fn main() {
    if DIFFS.is_none() {
        eprintln!("DFFY_BENCH_REPO unset, skipping benchmarks");
        return;
    }
    divan::main();
}

/// "renderer idle" has a renderer plugin configured for the file's type but
/// not asked for.
const CASES: [&str; 4] = [
    "table",
    "fair.c",
    "fair.c highlighted",
    "fair.c renderer idle",
];

fn frame(case: &str) -> Frame {
    let (largest, highlighted) = DIFFS.clone().expect("checked in main");
    let (diff, start) = if case == "table" {
        (largest, START)
    } else {
        (highlighted, 100)
    };
    let mut frame = if case.ends_with("renderer idle") {
        Frame::with_idle_renderer(diff, SIZE.0, SIZE.1)
    } else {
        Frame::new(diff, SIZE.0, SIZE.1, case.ends_with("highlighted"))
    };
    for _ in 0..start {
        frame.scroll(true);
    }
    frame.draw();
    frame.invalidate();
    eprintln!("{case}: a whole frame is {} bytes", frame.draw());
    frame
}

/// Drawing and flushing a whole screen, as after a resize.
#[divan::bench(args = CASES)]
fn full_frame(bencher: divan::Bencher, case: &str) {
    let mut frame = frame(case);
    bencher.bench_local(|| {
        frame.invalidate();
        frame.draw()
    });
}

/// One `j` or `k`: every row moves, so most cells change.
#[divan::bench(args = CASES)]
fn scroll_by_one(bencher: divan::Bencher, case: &str) {
    let mut frame = frame(case);
    let mut down = true;
    bencher.bench_local(|| {
        frame.scroll(down);
        down = !down;
        frame.draw()
    });
}

/// The workers' share of opening a file, run on the benchmark thread so the
/// allocation profiler counts it: the view, every hunk's emphasis and,
/// per case, highlighting and the syntax diff.
#[divan::bench(args = ["table", "fair.c", "fair.c highlighted", "fair.c syntax diff"])]
fn prepare_file(bencher: divan::Bencher, case: &str) {
    let (largest, highlighted) = DIFFS.clone().expect("checked in main");
    let diff = if case == "table" {
        largest
    } else {
        highlighted
    };
    bencher
        .with_inputs(|| diff.clone())
        .bench_local_values(|diff| {
            Frame::prepare(
                diff,
                case.ends_with("highlighted"),
                case.ends_with("syntax diff"),
            )
        });
}

/// The scope job's share of opening a highlighted file, after the parse
/// `prepare_file` counts: finding each side's definitions in its tree.
#[divan::bench(args = ["fair.c"])]
fn scopes(bencher: divan::Bencher, _case: &str) {
    let (_, highlighted) = DIFFS.clone().expect("checked in main");
    let trees = Trees::new(highlighted);
    bencher.bench_local(|| trees.scopes());
}

const FULL: [&str; 2] = ["table", "fair.c"];

/// A file switched to its full text with `X`, scrolled into it.
fn full(case: &str) -> Frame {
    let (largest, highlighted) = DIFFS.clone().expect("checked in main");
    let diff = if case == "table" {
        largest
    } else {
        highlighted
    };
    let mut frame = Frame::new(diff, SIZE.0, SIZE.1, false);
    frame.press('X');
    for _ in 0..START {
        frame.scroll(true);
    }
    frame.draw();
    frame
}

#[divan::bench(args = FULL)]
fn full_text_frame(bencher: divan::Bencher, case: &str) {
    let mut frame = full(case);
    bencher.bench_local(|| {
        frame.invalidate();
        frame.draw()
    });
}

#[divan::bench(args = FULL)]
fn full_text_scroll_by_one(bencher: divan::Bencher, case: &str) {
    let mut frame = full(case);
    let mut down = true;
    bencher.bench_local(|| {
        frame.scroll(down);
        down = !down;
        frame.draw()
    });
}

/// A file in file view (`l`), read on its new side, scrolled into it.
fn file_view(case: &str) -> Frame {
    let (largest, highlighted) = DIFFS.clone().expect("checked in main");
    let diff = if case == "table" {
        largest
    } else {
        highlighted
    };
    let mut frame = Frame::new(diff, SIZE.0, SIZE.1, false);
    frame.press('l');
    for _ in 0..START {
        frame.scroll(true);
    }
    frame.draw();
    frame
}

#[divan::bench(args = FULL)]
fn file_view_frame(bencher: divan::Bencher, case: &str) {
    let mut frame = file_view(case);
    bencher.bench_local(|| {
        frame.invalidate();
        frame.draw()
    });
}

#[divan::bench(args = FULL)]
fn file_view_scroll_by_one(bencher: divan::Bencher, case: &str) {
    let mut frame = file_view(case);
    let mut down = true;
    bencher.bench_local(|| {
        frame.scroll(down);
        down = !down;
        frame.draw()
    });
}

/// `l` in turn, off and on: the rows of the side read or of the diff,
/// each kept once built, to the frame it draws.
#[divan::bench(args = FULL)]
fn file_view_toggle(bencher: divan::Bencher, case: &str) {
    let mut frame = file_view(case);
    bencher.bench_local(|| {
        frame.press('l');
        frame.draw()
    });
}

/// A keystroke in the diff search, to the frame it draws: the pattern
/// grows to one character, then shrinks back to none; `0` is on nearly
/// every line of the table, `e` on fewer.
#[divan::bench(args = ['0', 'e'])]
fn search_keystroke(bencher: divan::Bencher, key: char) {
    let mut frame = frame("table");
    frame.press('/');
    frame.draw();
    let mut typed = false;
    bencher.bench_local(|| {
        if typed {
            frame.backspace();
        } else {
            frame.press(key);
        }
        typed = !typed;
        frame.draw()
    });
}

/// `n` and `N` in turn between the table's matches, each to the frame it
/// draws with the matches on screen lit.
#[divan::bench(args = ['0', 'e'])]
fn search_next_match(bencher: divan::Bencher, key: char) {
    let mut frame = frame("table");
    frame.press('/');
    frame.press(key);
    frame.find();
    frame.enter();
    frame.draw();
    let mut next = true;
    bencher.bench_local(|| {
        frame.press(if next { 'n' } else { 'N' });
        next = !next;
        frame.draw()
    });
}

/// The search's scan of the table for a one-character pattern: what a
/// worker does per keystroke, off the UI thread.
#[divan::bench(args = ['0', 'e'])]
fn search_scan(bencher: divan::Bencher, key: char) {
    let (largest, _) = DIFFS.clone().expect("checked in main");
    let pattern = key.to_string();
    eprintln!(
        "{key}: {} matches in the table's hunks",
        Frame::search(&largest, &pattern)
    );
    bencher.bench_local(|| Frame::search(&largest, &pattern));
}

/// The whole range in the file tree, showing fair.c.
fn tree() -> Frame {
    let (_, highlighted) = DIFFS.clone().expect("checked in main");
    let mut frame = Frame::with_tree(CHANGES.clone(), highlighted, SIZE.0, SIZE.1);
    frame.draw();
    frame
}

#[divan::bench]
fn tree_frame(bencher: divan::Bencher) {
    let mut frame = tree();
    bencher.bench_local(|| {
        frame.invalidate();
        frame.draw()
    });
}

/// `f` and `F` in turn: another file, and the tree's highlight moves.
#[divan::bench]
fn tree_next_file(bencher: divan::Bencher) {
    let mut frame = tree();
    let mut next = true;
    bencher.bench_local(|| {
        frame.press(if next { 'f' } else { 'F' });
        next = !next;
        frame.draw()
    });
}

/// The picker over the range's files and v6.1's other files.
fn picking() -> Picking {
    let root = PathBuf::from(std::env::var_os("DFFY_BENCH_REPO").expect("checked in main"));
    let repo = Repo::discover(&root).expect("bench repo opens");
    let spec = repo
        .resolve(&["v6.0", "v6.1"], false, false)
        .expect("tags resolve");
    let snapshots = spec.snapshots(false).expect("a range has snapshots");
    let mut picking = Picking::new(CHANGES.clone(), &repo, snapshots, SIZE);
    picking.draw();
    picking
}

/// The picker's first keystroke and the backspace after it, which scores
/// every file again, each to the frame it draws: the diff's files are
/// scored on this thread, the others handed to a worker
/// (`picker_score_78k`). `e` is in nearly every path.
#[divan::bench]
fn picker_keystroke_78k(bencher: divan::Bencher) {
    let mut picking = picking();
    let mut typed = false;
    bencher.bench_local(|| {
        if typed {
            picking.backspace();
        } else {
            picking.press('e');
        }
        typed = !typed;
        picking.draw()
    });
}

/// The picker opened, v6.1's files listed under the range's, and closed:
/// what closing lets go shows as deallocated bytes matching allocated.
#[divan::bench]
fn picker_open_close_78k(bencher: divan::Bencher) {
    let root = PathBuf::from(std::env::var_os("DFFY_BENCH_REPO").expect("checked in main"));
    let repo = Repo::discover(&root).expect("bench repo opens");
    let mut picking = picking();
    picking.close();
    bencher.bench_local(|| {
        picking.open(&repo);
        picking.close();
        picking.draw()
    });
}

/// The worker's share of the first keystroke: scoring v6.1's files the
/// range doesn't list.
#[divan::bench]
fn picker_score_78k(bencher: divan::Bencher) {
    let mut picking = picking();
    picking.press('e');
    let scoring = picking.scoring().expect("the other files wait for a score");
    eprintln!(
        "e: {} of {} other files match",
        scoring.run(),
        scoring.candidates()
    );
    bencher.bench_local(|| scoring.run());
}

/// The kernel's v6.1 files with the range's changes among them.
fn all_files() -> AllFiles {
    let root = PathBuf::from(std::env::var_os("DFFY_BENCH_REPO").expect("checked in main"));
    let repo = Repo::discover(&root).expect("bench repo opens");
    let spec = repo
        .resolve(&["v6.0", "v6.1"], false, false)
        .expect("tags resolve");
    let [_, new] = spec.snapshots(false).expect("a range has snapshots");
    AllFiles::new(CHANGES.clone(), &repo, new)
}

/// `a` in the tree: every file of v6.1, its top level made from the
/// cached list with the range's 13k changes merged in.
#[divan::bench]
fn tree_all_files_switch(bencher: divan::Bencher) {
    let all = all_files();
    eprintln!("all files: {} rows", all.switch());
    bencher.bench_local(|| all.switch());
}

/// The switch, then `drivers/` opened: its children made, about 30k
/// files under it walked. Opening it alone takes the difference.
#[divan::bench]
fn tree_open_dir(bencher: divan::Bencher) {
    let all = all_files();
    eprintln!("drivers/ open: {} rows", all.open("drivers"));
    bencher.bench_local(|| all.open("drivers"));
}

/// The file go to definition asks from, and the name: in its includes.
const PEEK_FROM: &str = "mm/slab_common.c";
const PEEK_NAME: &str = "kmalloc_array";

/// A viewer of the range on `PEEK_FROM`, its definitions kept in `cache`,
/// with `built` v6.1's index built.
fn peeking(cache: &tempfile::TempDir, built: bool) -> Peeking {
    let root = PathBuf::from(std::env::var_os("DFFY_BENCH_REPO").expect("checked in main"));
    let repo = Repo::discover(&root).expect("bench repo opens");
    let spec = repo
        .resolve(&["v6.0", "v6.1"], false, false)
        .expect("tags resolve");
    let snapshots = spec.snapshots(false).expect("a range has snapshots");
    let cache = Cache::new(cache.path().to_owned(), 1 << 30);
    Peeking::new(
        CHANGES.clone(),
        (&repo, PathBuf::from(env!("CARGO_BIN_EXE_dffy"))),
        snapshots,
        PEEK_FROM,
        (cache, built),
        SIZE,
    )
}

/// Peeking at `kmalloc_array` from `mm/slab_common.c`, to the frame
/// showing its definitions and the first one's code, the lookup and the
/// code's read and highlighting run on this thread. "built" has v6.1's
/// index built; "cold" starts with no index and an empty symbol cache, so
/// it lists v6.1's files and parses the asking file's includes.
#[divan::bench(args = ["built", "cold"], sample_count = 10, sample_size = 1)]
fn peek_open(bencher: divan::Bencher, case: &str) {
    let cache = tempfile::tempdir().expect("tempdir");
    if case == "built" {
        let mut peeking = peeking(&cache, true);
        eprintln!("peek: a frame is {} bytes", peeking.peek(PEEK_NAME));
        bencher.bench_local(|| peeking.peek(PEEK_NAME));
    } else {
        bencher
            .with_inputs(|| {
                let cache = tempfile::tempdir().expect("tempdir");
                let peeking = peeking(&cache, false);
                (cache, peeking)
            })
            .bench_local_values(|(_cache, mut peeking)| peeking.peek(PEEK_NAME));
    }
}

/// A whole frame with the peek open under the line.
#[divan::bench]
fn peek_frame(bencher: divan::Bencher) {
    let cache = tempfile::tempdir().expect("tempdir");
    let mut peeking = peeking(&cache, true);
    peeking.keep_open(PEEK_NAME);
    bencher.bench_local(|| peeking.full_frame());
}

/// The tree's count pass over the range on the viewer's pool, from an
/// empty cache: alone, and while v6.1's definitions index builds from an
/// empty symbol cache in the navigator helper, as the viewer has it.
#[divan::bench(args = ["alone", "indexing"], sample_count = 3, sample_size = 1)]
fn tree_count_pass(bencher: divan::Bencher, case: &str) {
    let root = PathBuf::from(std::env::var_os("DFFY_BENCH_REPO").expect("checked in main"));
    let repo = Repo::discover(&root).expect("bench repo opens");
    let spec = repo
        .resolve(&["v6.0", "v6.1"], false, false)
        .expect("tags resolve");
    let snapshots = spec.snapshots(false).expect("a range has snapshots");
    bencher
        .with_inputs(|| {
            let cache = tempfile::tempdir().expect("tempdir");
            let at = || Cache::new(cache.path().to_owned(), 1 << 30);
            let program = PathBuf::from(env!("CARGO_BIN_EXE_dffy"));
            let navigator =
                (case == "indexing").then(|| Frame::navigator(program, &repo, snapshots, &at()));
            (cache, navigator)
        })
        .bench_local_values(|(cache, navigator)| {
            let at = Cache::new(cache.path().to_owned(), 1 << 30);
            Frame::count_pass_beside(&repo, CHANGES.clone(), navigator.as_ref(), Some(at))
        });
}

/// A narrowing keystroke over as many other files as the picker scores
/// on the keystroke itself (its `SCORE_NOW`): `e` then `r`.
#[divan::bench]
fn picker_narrow_16k(bencher: divan::Bencher) {
    let mut picking = picking();
    picking.press('e');
    let first = picking.scoring().expect("the other files wait for a score");
    let narrowed = first.narrowed('r', 16_384);
    eprintln!(
        "er: {} of {} other files match",
        narrowed.run(),
        narrowed.candidates()
    );
    bencher.bench_local(|| narrowed.run());
}
