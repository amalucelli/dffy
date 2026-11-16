//! TUI frame time at 200×60 against the 8ms budget, on the Linux range
//! v6.0..v6.1: its largest file (too large to highlight), `kernel/sched/fair.c`
//! with and without highlighting, and the file tree over the whole range.
//! Needs the kernel clone named by `DFFY_BENCH_REPO`; skipped without it.
//! Timings are reported, never asserted.
#![expect(
    clippy::expect_used,
    reason = "benchmark setup: a failed step leaves nothing to measure"
)]

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::LazyLock;

use dffy::bench::Frame;
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
