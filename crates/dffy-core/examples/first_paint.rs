//! Time to first results from process start, which in-process benchmarks
//! can't show (binary load, repository and pack-index open, cold allocator).
//!
//! ```sh
//! cargo run --release --example first_paint -- <repo> range|tracked|worktree
//! ```
#![expect(
    clippy::expect_used,
    reason = "a measuring tool: a failed step ends the run with its message"
)]

use std::ops::ControlFlow;
use std::path::Path;
use std::time::Instant;

use dffy_core::git::{DiffSpec, Repo, Status};

fn main() {
    let start = Instant::now();
    let mut args = std::env::args().skip(1);
    let (Some(root), Some(mode)) = (args.next(), args.next()) else {
        eprintln!("usage: first_paint <repo> range|tracked|worktree");
        std::process::exit(2);
    };
    let ms = || start.elapsed().as_secs_f64() * 1e3;
    let repo = Repo::discover(Path::new(&root)).expect("repository opens");
    let opened = ms();
    let (spec, marks): (DiffSpec, &[usize]) = match mode.as_str() {
        "tracked" => (DiffSpec::IndexToWorktree { untracked: false }, &[1]),
        "range" => (
            repo.resolve(&["v6.0..v6.1"], false, false)
                .expect("tags resolve"),
            &[1, 50],
        ),
        _ => (DiffSpec::IndexToWorktree { untracked: true }, &[1]),
    };
    let resolved = ms();
    let mut n = 0;
    let mut stamps = Vec::new();
    let mut tracked_done = None;
    repo.for_each_change(&spec, &[], |c| {
        n += 1;
        if marks.contains(&n) {
            stamps.push((n, ms()));
        }
        if c.status == Status::Untracked && tracked_done.is_none() {
            tracked_done = Some(ms());
        }
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let done = ms();
    let stamps: Vec<String> = stamps
        .iter()
        .map(|(k, t)| format!("first_{k}={t:.2}"))
        .collect();
    println!(
        "open={opened:.2} resolve={resolved:.2} {} tracked_done={} all={done:.2} changes={n}",
        stamps.join(" "),
        tracked_done.map_or_else(|| "-".to_owned(), |t| format!("{t:.2}")),
    );
}
