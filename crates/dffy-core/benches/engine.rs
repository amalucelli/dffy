//! Engine benchmarks on the Linux kernel clone named by `DFFY_BENCH_REPO`
//! (tags v6.0 and v6.1 fetched). Without it the benchmarks are skipped.
//! Timings are reported, never asserted.
#![expect(
    clippy::expect_used,
    reason = "benchmark setup: a failed step leaves nothing to measure"
)]

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::LazyLock;

use dffy_core::diff::{self, Options, Text, Whitespace};
use dffy_core::git::{DiffSpec, FileChange, Repo, Side, Status};

const LARGEST: &str = "drivers/net/wireless/realtek/rtw89/rtw8852c_table.c";

struct Pair {
    old: Vec<u8>,
    new: Vec<u8>,
}

struct Fixture {
    root: PathBuf,
    largest: Pair,
    median: Pair,
    changes: [FileChange; 2],
}

static FIXTURE: LazyLock<Option<Fixture>> = LazyLock::new(|| {
    let root = PathBuf::from(std::env::var_os("DFFY_BENCH_REPO")?);
    let repo = Repo::discover(&root).expect("bench repo opens");
    let spec = repo
        .resolve(&["v6.0", "v6.1"], false, false)
        .expect("tags resolve");
    let mut modified_c = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        if c.status == Status::Modified && c.path.ends_with(b".c") {
            modified_c.push(c);
        }
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let load = |c: &FileChange| {
        let side = |s: Option<Side>| {
            repo.load(c.path.as_ref(), &s.expect("both sides"))
                .expect("loads")
        };
        Pair {
            old: side(c.old),
            new: side(c.new),
        }
    };
    let largest = modified_c
        .iter()
        .find(|c| c.path == LARGEST)
        .expect("largest file changed");
    let mut sized: Vec<_> = modified_c.iter().map(|c| (load(c).new.len(), c)).collect();
    sized.sort_by_key(|(len, _)| *len);
    let median = sized[sized.len() / 2].1;
    Some(Fixture {
        root,
        largest: load(largest),
        median: load(median),
        changes: [largest.clone(), median.clone()],
    })
});

fn main() {
    if FIXTURE.is_none() {
        eprintln!("DFFY_BENCH_REPO unset, skipping benchmarks");
        return;
    }
    divan::main();
}

fn fixture() -> &'static Fixture {
    FIXTURE.as_ref().expect("checked in main")
}

fn pair(name: &str) -> &'static Pair {
    match name {
        "largest" => &fixture().largest,
        _ => &fixture().median,
    }
}

const FILES: [&str; 2] = ["largest", "median"];

/// Interning plus histogram, with the line split done up front.
#[divan::bench(args = FILES)]
fn dffy_histogram(bencher: divan::Bencher, file: &str) {
    let p = pair(file);
    let (old, new) = (Text::new(p.old.clone()), Text::new(p.new.clone()));
    bencher.bench(|| diff::changes(&old, &new, Whitespace::Exact));
}

/// Same, including the line split (and the copy into an owned buffer).
#[divan::bench(args = FILES)]
fn dffy_histogram_from_bytes(bencher: divan::Bencher, file: &str) {
    let p = pair(file);
    bencher.bench(|| {
        let (old, new) = (Text::new(p.old.clone()), Text::new(p.new.clone()));
        diff::changes(&old, &new, Whitespace::Exact)
    });
}

/// Line split, interning and histogram, without imara's slider postprocessing
/// (dffy has none yet).
#[divan::bench(args = FILES)]
fn imara_histogram(bencher: divan::Bencher, file: &str) {
    let p = pair(file);
    bencher.bench(|| {
        let input = imara_diff::InternedInput::new(&p.old[..], &p.new[..]);
        imara_diff::Diff::compute(imara_diff::Algorithm::Histogram, &input)
    });
}

/// A file as print mode and the viewer diff it: both blobs loaded, the line
/// diff into hunks, then each hunk's lines paired, which finds their
/// emphasis too.
#[divan::bench(args = FILES)]
fn dffy_file_diff_with_intraline(bencher: divan::Bencher, file: &str) {
    // Without the object cache, as print mode's workers read blobs.
    let repo = Repo::discover(&fixture().root)
        .expect("opens")
        .without_object_cache();
    let change = &fixture().changes[usize::from(file != "largest")];
    bencher.bench_local(|| {
        let mut fd = repo
            .file_diff(change.clone(), &Options::default())
            .expect("diffs");
        let emphasis: Vec<_> = fd
            .hunks
            .iter_mut()
            .map(|hunk| diff::pair(&fd.old, &fd.new, hunk))
            .collect();
        (fd, emphasis)
    });
}

/// Enumerates the changes of `revs`, stopping after `limit`, with the
/// repository opened fresh each time as a new process would.
fn enumerate(revs: &[&str], limit: usize) -> usize {
    let repo = Repo::discover(&fixture().root).expect("opens");
    let spec = repo.resolve(revs, false, false).expect("resolves");
    count(&repo, &spec, limit)
}

fn count(repo: &Repo, spec: &DiffSpec, limit: usize) -> usize {
    let mut n = 0;
    repo.for_each_change(spec, &[], |_| {
        n += 1;
        if n >= limit {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .expect("enumerates");
    n
}

#[divan::bench(sample_count = 30)]
fn range_first_50_paths() -> usize {
    enumerate(&["v6.0..v6.1"], 50)
}

#[divan::bench(sample_count = 30)]
fn range_all_paths() -> usize {
    enumerate(&["v6.0..v6.1"], usize::MAX)
}

fn worktree(untracked: bool, limit: usize) -> usize {
    let repo = Repo::discover(&fixture().root).expect("opens");
    count(&repo, &DiffSpec::IndexToWorktree { untracked }, limit)
}

#[divan::bench(sample_count = 30)]
fn worktree_first_modified() -> usize {
    worktree(false, 1)
}

#[divan::bench(sample_count = 30)]
fn worktree_all_tracked() -> usize {
    worktree(false, usize::MAX)
}

#[divan::bench(sample_count = 30)]
fn worktree_with_untracked() -> usize {
    worktree(true, usize::MAX)
}
