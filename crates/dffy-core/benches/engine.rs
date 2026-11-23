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
use std::sync::atomic::AtomicBool;

use dffy_core::diff::{self, Options, Text, Whitespace};
use dffy_core::git::{DiffSpec, FileChange, FileDiff, Repo, Side, Status};
use dffy_core::highlight::{self, Registry};
use dffy_core::view::{self, LineState};

// Allocation counts are deterministic, so `cargo xtask perf` gates on them
// with no noise band. Only the benchmark thread is counted.
#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const LARGEST: &str = "drivers/net/wireless/realtek/rtw89/rtw8852c_table.c";

struct Pair {
    old: Vec<u8>,
    new: Vec<u8>,
}

// Each input loads on first use, so a process that runs one benchmark, as
// `cargo xtask perf` runs them, loads only what that benchmark reads.
static ROOT: LazyLock<Option<PathBuf>> =
    LazyLock::new(|| std::env::var_os("DFFY_BENCH_REPO").map(PathBuf::from));

/// The range's modified C files the file benchmarks read: the largest, and
/// the median by the new side's size.
static CHANGES: LazyLock<[FileChange; 2]> = LazyLock::new(|| {
    let repo = Repo::discover(root()).expect("bench repo opens");
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
    let largest = modified_c
        .iter()
        .find(|c| c.path == LARGEST)
        .expect("largest file changed");
    // A blob's size from its object header is its length once loaded.
    let mut sized: Vec<_> = modified_c
        .iter()
        .map(|c| (repo.sizes(c).expect("sizes")[1], c))
        .collect();
    sized.sort_by_key(|(len, _)| *len);
    let median = sized[sized.len() / 2].1;
    [largest.clone(), median.clone()]
});

fn load(change: &FileChange) -> Pair {
    let repo = Repo::discover(root()).expect("bench repo opens");
    let side = |s: Option<Side>| {
        repo.load(change.path.as_ref(), &s.expect("both sides"))
            .expect("loads")
    };
    Pair {
        old: side(change.old),
        new: side(change.new),
    }
}

static LARGEST_PAIR: LazyLock<Pair> = LazyLock::new(|| load(&CHANGES[0]));
static MEDIAN_PAIR: LazyLock<Pair> = LazyLock::new(|| load(&CHANGES[1]));

fn main() {
    if ROOT.is_none() {
        eprintln!("DFFY_BENCH_REPO unset, skipping benchmarks");
        return;
    }
    // glibc serves allocations above its mmap threshold with fresh pages,
    // faulted in on every use, and raises the threshold to the size of a
    // freed mapped block. dffy reads many blobs before it diffs any one, so
    // freeing one large block here puts the benchmarks' multi-megabyte
    // buffers on the reused heap, as in a real run.
    drop(std::hint::black_box(vec![0_u8; 16 << 20]));
    divan::main();
}

fn root() -> &'static PathBuf {
    ROOT.as_ref().expect("checked in main")
}

fn pair(name: &str) -> &'static Pair {
    match name {
        "largest" => &LARGEST_PAIR,
        _ => &MEDIAN_PAIR,
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
    let repo = Repo::discover(root())
        .expect("opens")
        .without_object_cache();
    let change = &CHANGES[usize::from(file != "largest")];
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

const FAIR: &str = "kernel/sched/fair.c";

/// kernel/sched/fair.c's line diff over the range, its hunks paired as the
/// viewer holds them.
static FAIR_DIFF: LazyLock<FileDiff> = LazyLock::new(|| {
    let repo = Repo::discover(root()).expect("bench repo opens");
    let spec = repo
        .resolve(&["v6.0", "v6.1"], false, false)
        .expect("tags resolve");
    let mut fair = None;
    repo.for_each_change(&spec, &[], |c| {
        if c.path == FAIR {
            fair = Some(c);
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let change = fair.expect("fair.c changed");
    let mut fd = repo.file_diff(change, &Options::default()).expect("diffs");
    for hunk in &mut fd.hunks {
        diff::pair(&fd.old, &fd.new, hunk);
    }
    fd
});

/// The rows of every hunk of kernel/sched/fair.c read on one side alone,
/// as turning file view on lays out a file.
#[divan::bench(args = ["old", "new"])]
fn side_rows(bencher: divan::Bencher, side: &str) {
    let side = if side == "old" {
        view::Side::Old
    } else {
        view::Side::New
    };
    let kinds: Vec<Vec<(view::Kind, bool)>> = (FAIR_DIFF.hunks.iter())
        .map(|h| {
            (h.lines.iter())
                .map(|l| (LineState::of(l).kind(), l.block_start))
                .collect()
        })
        .collect();
    bencher.bench_local(|| {
        let mut shown = 0;
        for hunk in &kinds {
            let mut rows = Vec::new();
            view::side_rows(side, hunk.len(), |i| hunk[i], &mut rows);
            shown += rows.len();
        }
        shown
    });
}

/// Enumerates the changes of `revs`, stopping after `limit`, with the
/// repository opened fresh each time as a new process would.
fn enumerate(revs: &[&str], limit: usize) -> usize {
    let repo = Repo::discover(root()).expect("opens");
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
    let repo = Repo::discover(root()).expect("opens");
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

/// Every path of v6.1's tree, as opening a file outside the diff lists
/// them, with the repository opened fresh each time.
#[divan::bench(sample_count = 30)]
fn list_paths(bencher: divan::Bencher) {
    let repo = Repo::discover(root()).expect("opens");
    let spec = repo
        .resolve(&["v6.0..v6.1"], false, false)
        .expect("resolves");
    let [_, new] = spec.snapshots(false).expect("a range has snapshots");
    bencher.bench_local(|| {
        let repo = Repo::discover(root()).expect("opens");
        let mut n = 0_usize;
        repo.list_paths(new, &[], |_, _| {
            n += 1;
            ControlFlow::Continue(())
        })
        .expect("lists");
        n
    });
}

/// The files `extract_definitions` reads at v6.1: a large source file, and
/// a header that is mostly `#define`s.
const DEFINING: [&str; 2] = [FAIR, "drivers/gpu/drm/i915/i915_reg.h"];

/// A file's definitions as the symbol index reads a blob: parsed for them,
/// then walked outside function bodies, with the macro table's.
#[divan::bench(args = DEFINING)]
fn extract_definitions(bencher: divan::Bencher, path: &str) {
    let repo = Repo::discover(root()).expect("opens");
    let spec = repo
        .resolve(&["v6.0..v6.1"], false, false)
        .expect("resolves");
    let [_, new] = spec.snapshots(false).expect("a range has snapshots");
    let side = repo
        .side_at(new, path.into())
        .expect("reads")
        .expect("a file");
    let text = repo.load(path.into(), &side).expect("loads");
    let grammars = Registry::builtin();
    let c = grammars.named("c").expect("bundled");
    highlight::definitions(c, &text)
        .expect("the query compiles")
        .expect("a tags query")
        .expect("under the size cap and the parse budget");
    bencher.bench_local(|| highlight::definitions(c, &text));
}

/// Each language's tags fixture but C's and Rust's, by its language. The
/// kernel has none of them, so their files are the fixture repeated.
const FIXTURES: [(&str, &str); 17] = [
    (
        "typescript",
        include_str!("../src/highlight/tags/fixtures/fixture.ts"),
    ),
    (
        "tsx",
        include_str!("../src/highlight/tags/fixtures/fixture.ts"),
    ),
    (
        "javascript",
        include_str!("../src/highlight/tags/fixtures/fixture.js"),
    ),
    (
        "python",
        include_str!("../src/highlight/tags/fixtures/fixture.py"),
    ),
    (
        "go",
        include_str!("../src/highlight/tags/fixtures/fixture.go"),
    ),
    (
        "java",
        include_str!("../src/highlight/tags/fixtures/fixture.java"),
    ),
    (
        "cpp",
        include_str!("../src/highlight/tags/fixtures/fixture.cpp"),
    ),
    (
        "csharp",
        include_str!("../src/highlight/tags/fixtures/fixture.cs"),
    ),
    (
        "kotlin",
        include_str!("../src/highlight/tags/fixtures/fixture.kt"),
    ),
    (
        "swift",
        include_str!("../src/highlight/tags/fixtures/fixture.swift"),
    ),
    (
        "ruby",
        include_str!("../src/highlight/tags/fixtures/fixture.rb"),
    ),
    (
        "php",
        include_str!("../src/highlight/tags/fixtures/fixture.php"),
    ),
    (
        "scala",
        include_str!("../src/highlight/tags/fixtures/fixture.scala"),
    ),
    (
        "zig",
        include_str!("../src/highlight/tags/fixtures/fixture.zig"),
    ),
    (
        "lua",
        include_str!("../src/highlight/tags/fixtures/fixture.lua"),
    ),
    (
        "elixir",
        include_str!("../src/highlight/tags/fixtures/fixture.ex"),
    ),
    (
        "bash",
        include_str!("../src/highlight/tags/fixtures/fixture.sh"),
    ),
];

/// The languages `extract_definitions_of` reads.
const LANGUAGES: [&str; 17] = [
    "typescript",
    "tsx",
    "javascript",
    "python",
    "go",
    "java",
    "cpp",
    "csharp",
    "kotlin",
    "swift",
    "ruby",
    "php",
    "scala",
    "zig",
    "lua",
    "elixir",
    "bash",
];

/// [`extract_definitions`] for a file of each other language with a tags
/// query: its fixture, then its body repeated to about 256KB, half the size
/// cap, with its throughput. The body leaves out the lines a file has only
/// at its top (`package`, `import`, `<?php`), which parse as errors after
/// it, and a parse recovering from errors would be measured instead.
#[divan::bench(args = LANGUAGES)]
fn extract_definitions_of(bencher: divan::Bencher, language: &str) {
    const TOP: [&str; 4] = ["package ", "import ", "<?php", "use "];
    let fixture = FIXTURES
        .iter()
        .find(|(name, _)| *name == language)
        .expect("a fixture")
        .1;
    let body: String = (fixture.lines())
        .filter(|line| !TOP.iter().any(|top| line.starts_with(top)))
        .flat_map(|line| [line, "\n"])
        .collect();
    let text = [fixture, &body.repeat((256 << 10) / body.len())].concat();
    let grammars = Registry::builtin();
    let lang = grammars.named(language).expect("bundled");
    highlight::definitions(lang, text.as_bytes())
        .expect("the query compiles")
        .expect("a tags query")
        .expect("under the size cap and the parse budget");
    bencher
        .counter(divan::counter::BytesCount::new(text.len()))
        .bench_local(|| highlight::definitions(lang, text.as_bytes()));
}

/// A lookup from `mm/slab_common.c` at v6.1 once the index is built: a
/// name defined in a few files, and one defined in thousands. The index is
/// read from a symbol cache kept across runs in the temporary directory, so
/// only the first run extracts definitions.
#[divan::bench(args = ["kmalloc_array", "pr_fmt"])]
fn lookup_warm(bencher: divan::Bencher, name: &str) {
    use dffy_core::cache::Cache;
    use dffy_core::navigate::{Diff, Navigator, Origin, Shared};

    let repo = Repo::discover(root()).expect("opens");
    let spec = repo
        .resolve(&["v6.0..v6.1"], false, false)
        .expect("resolves");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let grammars = Registry::builtin();
    let diff = Diff {
        snapshots: spec.snapshots(false).expect("a range has snapshots"),
        changes: &changes,
        options: Options::default(),
    };
    let nav =
        Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New).expect("lists");
    let cache = Cache::new(std::env::temp_dir().join("dffy-bench-symbols"), u64::MAX).symbols();
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    nav.build(&repo, &cache, threads, &AtomicBool::new(false), |_| {})
        .expect("the queries compile");
    let origin = Origin {
        path: "mm/slab_common.c".into(),
        language: grammars.named("c").expect("bundled").clone(),
    };
    let go = AtomicBool::new(false);
    nav.lookup(&repo, &cache, name, &origin, &go)
        .expect("looks up");
    bencher.bench_local(|| nav.lookup(&repo, &cache, name, &origin, &go));
}

/// Searching v6.1 for the uses of a name, asked from `mm/slab_common.c`,
/// with no index built: to the first 200 files with a use (what the peek
/// lists), and through every file (its count). `list_head` is used in
/// thousands of files, `sched_setattr_nocheck` in a handful.
#[divan::bench(
    args = [
        "list_head first 200",
        "list_head all",
        "sched_setattr_nocheck first 200",
        "sched_setattr_nocheck all",
    ],
    sample_count = 5,
    sample_size = 1
)]
fn references_kernel(bencher: divan::Bencher, case: &str) {
    use dffy_core::navigate::{Diff, Filter, MAX_REFERENCE_FILES, Navigator, Origin, Shared};

    let (name, until) = case.split_once(' ').expect("a name and how far");
    let repo = Repo::discover(root()).expect("opens");
    let spec = repo
        .resolve(&["v6.0..v6.1"], false, false)
        .expect("resolves");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let grammars = Registry::builtin();
    let diff = Diff {
        snapshots: spec.snapshots(false).expect("a range has snapshots"),
        changes: &changes,
        options: Options::default(),
    };
    let nav =
        Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New).expect("lists");
    let origin = Origin {
        path: "mm/slab_common.c".into(),
        language: grammars.named("c").expect("bundled").clone(),
    };
    let go = AtomicBool::new(false);
    let search = || {
        let mut search = nav
            .references(&repo, name, &origin, Filter::default(), &go)
            .expect("starts");
        let mut hits = 0;
        while !search.done() && (until == "all" || search.files() < MAX_REFERENCE_FILES) {
            let mut files = 0;
            hits += search
                .step(&nav, &repo, || {
                    files += 1;
                    files > 16
                })
                .len();
        }
        (search.files(), hits)
    };
    let (files, hits) = search();
    eprintln!("{case}: {files} files, {hits} lines listed");
    bencher.bench_local(search);
}
