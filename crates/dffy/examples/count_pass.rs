//! Time of the file tree's count pass over `v6.0..v6.1` on the kernel
//! clone named by `DFFY_BENCH_REPO`, on the viewer's worker pool, and a
//! check that every count equals the line diff's, so a count never
//! changes when the file's diff loads. Run a release build:
//!
//! ```sh
//! cargo run --release --example count_pass
//! ```
#![expect(
    clippy::expect_used,
    reason = "a measuring tool: a failed step ends the run with its message"
)]

use std::ops::ControlFlow;
use std::time::Instant;

use dffy::bench::Frame;
use dffy_core::diff::{LineKind, Options};
use dffy_core::git::Repo;

const RUNS: usize = 3;

fn main() {
    let Some(root) = std::env::var_os("DFFY_BENCH_REPO") else {
        eprintln!("DFFY_BENCH_REPO unset, skipping");
        return;
    };
    let repo = Repo::discover(std::path::Path::new(&root)).expect("the clone opens");
    let spec = repo
        .resolve(&["v6.0", "v6.1"], false, false)
        .expect("tags resolve");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let mut times = Vec::new();
    let mut counts = Vec::new();
    for _ in 0..RUNS {
        let start = Instant::now();
        counts = Frame::count_pass(&repo, changes.clone());
        times.push(start.elapsed());
    }
    times.sort();
    println!(
        "count pass over {} files: median {:?}, min {:?}, max {:?} over {RUNS} runs",
        changes.len(),
        times[RUNS / 2],
        times[0],
        times[RUNS - 1]
    );
    let mut differ = 0;
    for (change, &(added, removed)) in changes.into_iter().zip(&counts) {
        let path = change.path.clone();
        let fd = repo.file_diff(change, &Options::default()).expect("diffs");
        let lines = || fd.hunks.iter().flat_map(|h| &h.lines);
        let from_diff = (
            lines().filter(|l| l.kind == LineKind::Added).count(),
            lines().filter(|l| l.kind == LineKind::Removed).count(),
        );
        if from_diff != (added, removed) {
            differ += 1;
            eprintln!(
                "{path}: counted {added}/{removed}, diffed {}/{}",
                from_diff.0, from_diff.1
            );
        }
    }
    println!("{differ} files whose count differs from their diff");
}
