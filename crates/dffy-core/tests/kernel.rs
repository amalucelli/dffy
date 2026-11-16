//! Checks against the Linux kernel clone named by `DFFY_BENCH_REPO` (tags
//! v6.0 and v6.1 fetched). Each test passes trivially when the variable is
//! unset, so `task ci` doesn't need the clone.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;

use dffy_core::diff::{Hunk, LineKind, Options, Text, Unchanged};
use dffy_core::git::{DiffSpec, FileChange, Repo, Status};

const LARGEST: &str = "drivers/net/wireless/realtek/rtw89/rtw8852c_table.c";

// dffy reports fewer changed lines than git --histogram on these, so prove the hunks still apply.
const SMALLER_THAN_GIT: [&str; 3] = [
    "Documentation/devicetree/bindings/net/dsa/mediatek,mt7530.yaml",
    "arch/loongarch/kernel/dma.c",
    "tools/lib/find_bit.c",
];

fn bench_repo() -> Option<PathBuf> {
    let path = std::env::var_os("DFFY_BENCH_REPO").map(PathBuf::from);
    if path.is_none() {
        eprintln!("DFFY_BENCH_REPO unset, skipping");
    }
    path
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).expect("utf-8")
}

fn collect(repo: &Repo, spec: &DiffSpec) -> Vec<FileChange> {
    let mut out = Vec::new();
    repo.for_each_change(spec, &[], |c| {
        out.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    out
}

fn letter(status: &Status) -> &'static str {
    match status {
        Status::Added => "A",
        Status::Deleted => "D",
        Status::Modified => "M",
        Status::TypeChange => "T",
        Status::Untracked => "??",
        Status::Renamed { .. } => "R",
    }
}

fn ours(changes: &[FileChange]) -> Vec<(String, String)> {
    changes
        .iter()
        .map(|c| (letter(&c.status).to_owned(), c.path.to_string()))
        .collect()
}

fn name_status(out: &str) -> Vec<(String, String)> {
    let fields: Vec<&str> = out.split('\0').filter(|s| !s.is_empty()).collect();
    fields
        .chunks(2)
        .map(|c| (c[0].to_owned(), c[1].to_owned()))
        .collect()
}

#[test]
fn range_matches_git_name_status() {
    let Some(dir) = bench_repo() else { return };
    let repo = Repo::discover(&dir).expect("repo");
    let spec = repo.resolve(&["v6.0..v6.1"], false, false).expect("spec");
    let expected = name_status(&git(
        &dir,
        &[
            "diff",
            "--no-ext-diff",
            "--name-status",
            "--no-renames",
            "-z",
            "v6.0",
            "v6.1",
        ],
    ));
    let got = ours(&collect(&repo, &spec));
    assert_eq!(got.len(), expected.len());
    assert_eq!(got, expected);
}

#[test]
fn worktree_matches_git_status() {
    let Some(dir) = bench_repo() else { return };
    let repo = Repo::discover(&dir).expect("repo");
    let status = git(
        &dir,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
        ],
    );
    let mut worktree = Vec::new();
    let mut staged = Vec::new();
    for entry in status.split('\0').filter(|s| s.len() > 3) {
        let (x, y, path) = (&entry[..1], &entry[1..2], entry[3..].to_owned());
        if &entry[..2] == "??" {
            worktree.push(("??".to_owned(), path));
            continue;
        }
        if y != " " {
            worktree.push((y.to_owned(), path.clone()));
        }
        if x != " " {
            staged.push((x.to_owned(), path));
        }
    }
    let got = ours(&collect(
        &repo,
        &DiffSpec::IndexToWorktree { untracked: true },
    ));
    assert_eq!(got, worktree);
    let spec = repo.resolve(&[], true, false).expect("staged spec");
    assert_eq!(ours(&collect(&repo, &spec)), staged);
}

/// `R<score>\told\tnew` lines of `git diff -M --name-status -z`.
fn git_renames(out: &str) -> Vec<String> {
    let mut fields = out.split('\0').filter(|s| !s.is_empty());
    let mut rows = Vec::new();
    while let Some(status) = fields.next() {
        if status.starts_with('R') {
            let (old, new) = (fields.next().expect("old"), fields.next().expect("new"));
            rows.push(format!("{status}\t{old}\t{new}"));
        } else {
            fields.next();
        }
    }
    rows
}

/// Our rename pass against git's on the range; reports matches, extras,
/// misses and time. Allows a handful of differences.
#[test]
fn range_renames_match_git() {
    let Some(dir) = bench_repo() else { return };
    let repo = Repo::discover(&dir).expect("repo");
    let spec = repo.resolve(&["v6.0..v6.1"], false, false).expect("spec");
    let changes = collect(&repo, &spec);
    let mut times = Vec::new();
    let mut ours = Vec::new();
    for _ in 0..5 {
        let start = std::time::Instant::now();
        let renamed = repo.detect_renames(changes.clone(), None).expect("renames");
        times.push(start.elapsed());
        ours = renamed
            .iter()
            .filter_map(|c| match &c.status {
                Status::Renamed { from, similarity } => {
                    Some(format!("R{similarity:03}\t{from}\t{}", c.path))
                }
                _ => None,
            })
            .collect();
    }
    times.sort();
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = dffy_core::cache::Cache::new(cache_dir.path().to_owned(), 1 << 30);
    let run_cached = || {
        let start = std::time::Instant::now();
        let renamed = repo
            .detect_renames(changes.clone(), Some(&cache))
            .expect("renames");
        (start.elapsed(), renamed)
    };
    let (miss, fresh) = run_cached();
    let (hit, cached) = run_cached();
    assert_eq!(fresh, cached);
    eprintln!("rename cache: miss {miss:?}, hit {hit:?}");
    let start = std::time::Instant::now();
    let expected = git_renames(&git(
        &dir,
        &[
            "diff",
            "--no-ext-diff",
            "-M",
            "--name-status",
            "-z",
            "v6.0",
            "v6.1",
        ],
    ));
    let git_time = start.elapsed();
    let pairs = |rows: &[String]| -> Vec<String> {
        rows.iter()
            .map(|r| {
                r.split_once('\t')
                    .map_or_else(|| r.clone(), |(_, p)| p.to_owned())
            })
            .collect()
    };
    let (our_pairs, git_pairs) = (pairs(&ours), pairs(&expected));
    let extra: Vec<_> = our_pairs
        .iter()
        .filter(|p| !git_pairs.contains(p))
        .collect();
    let missing: Vec<_> = git_pairs
        .iter()
        .filter(|p| !our_pairs.contains(p))
        .collect();
    let score_differs = ours.iter().filter(|r| !expected.contains(r)).count() - extra.len();
    eprintln!(
        "renames: ours {} git {}; same pairs {}, extra {}, missing {}, scores differ {}; \
         ours median {:?} (min {:?}), git CLI {git_time:?}",
        ours.len(),
        expected.len(),
        our_pairs.len() - extra.len(),
        extra.len(),
        missing.len(),
        score_differs,
        times[times.len() / 2],
        times[0],
    );
    eprintln!("extra: {extra:#?}\nmissing: {missing:#?}");
    assert!(extra.len() + missing.len() <= 6);
}

/// Rebuilds the new text from the old text and the hunks.
fn apply(old: &Text, new: &Text, hunks: &[Hunk]) -> Vec<u8> {
    let mut out = Vec::with_capacity(new.bytes().len());
    let mut o = 0;
    for h in hunks {
        while o < h.old_start {
            out.extend_from_slice(old.line(o));
            o += 1;
        }
        let mut n = h.new_start;
        for line in &h.lines {
            assert_eq!((line.old, line.new), (o, n), "lines in order on both sides");
            match line.kind {
                LineKind::Context => {
                    assert_eq!(old.line(line.old), new.line(line.new));
                    out.extend_from_slice(old.line(line.old));
                    o += 1;
                    n += 1;
                }
                LineKind::Removed => o += 1,
                LineKind::Added => {
                    out.extend_from_slice(new.line(line.new));
                    n += 1;
                }
            }
        }
    }
    while o < old.len() {
        out.extend_from_slice(old.line(o));
        o += 1;
    }
    out
}

#[test]
fn hunks_reproduce_new_on_sampled_files() {
    let Some(dir) = bench_repo() else { return };
    let repo = Repo::discover(&dir).expect("repo");
    let spec = repo.resolve(&["v6.0", "v6.1"], false, false).expect("spec");
    let modified: Vec<_> = collect(&repo, &spec)
        .into_iter()
        .filter(|c| c.status == Status::Modified)
        .collect();
    let step = (modified.len() / 250).max(1);
    let sample: Vec<_> = modified
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            i % step == 0 || c.path == LARGEST || SMALLER_THAN_GIT.iter().any(|p| c.path == *p)
        })
        .map(|(_, c)| c.clone())
        .collect();
    assert!(sample.len() >= 200, "sampled {}", sample.len());
    assert!(sample.iter().any(|c| c.path == LARGEST));
    for p in SMALLER_THAN_GIT {
        assert!(sample.iter().any(|c| c.path == p), "{p} not in v6.0..v6.1");
    }

    let mut diffed = 0;
    for change in sample {
        let path = change.path.to_string();
        let fd = repo
            .file_diff(change, &Options::default())
            .expect("file diff");
        if fd.binary || fd.too_large {
            continue;
        }
        assert_eq!(apply(&fd.old, &fd.new, &fd.hunks), fd.new.bytes(), "{path}");
        diffed += 1;
    }
    assert!(diffed >= 200, "diffed {diffed}");
}

/// What a watch refresh of one edited file costs on the kernel tree, with
/// the index already loaded. Reads only; timings are reported, not asserted.
#[test]
fn refreshing_one_path_reuses_the_loaded_index() {
    let Some(dir) = bench_repo() else { return };
    let repo = Repo::discover(&dir).expect("repo");
    let revisions = dffy_core::git::Revisions::default();
    let spec = repo.resolve_revisions(&revisions).expect("spec");
    let mut set = dffy_core::git::ChangeSet::new(repo, spec, revisions, &[], false);
    set.enumerate(|_| ControlFlow::Continue(()))
        .expect("enumerate");
    let before = set.files();
    for path in ["kernel/exit.c", "drivers/gpu/drm/drm_gem.c", "README"] {
        let mut times: Vec<_> = (0..20)
            .map(|_| {
                let start = std::time::Instant::now();
                set.refresh(&[path.into()]).expect("refresh");
                start.elapsed()
            })
            .collect();
        times.sort();
        eprintln!(
            "refresh {path}: median {:?}, max {:?}",
            times[times.len() / 2],
            times[times.len() - 1]
        );
    }
    assert_eq!(set.files(), before);
}

/// Kernel files with large changes from v6.0 to v6.1, where difftastic falls
/// back to a line diff on every one.
const RESEARCH_FILES: [&str; 5] = [
    "kernel/exit.c",
    "mm/mmap.c",
    "mm/vmscan.c",
    "kernel/sched/fair.c",
    LARGEST,
];

/// Resident memory in bytes.
fn rss() -> usize {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<usize>().ok())
        .map_or(0, |pages| pages * 4096)
}

/// The process's peak resident memory in bytes since the last
/// [`reset_peak`].
fn peak() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
            line.split_whitespace().nth(1)?.parse::<usize>().ok()
        })
        .map_or(0, |kb| kb * 1024)
}

fn reset_peak() {
    // Linux resets the peak to the current resident size on "5".
    std::fs::write("/proc/self/clear_refs", "5").ok();
}

/// Changed lines shown as changes (neither dimmed nor marked moved), lines
/// marked moved, and emphasized ranges.
fn shown(hunks: &[Hunk]) -> (usize, usize, usize) {
    let lines = hunks.iter().flat_map(|h| &h.lines);
    let changed: Vec<_> = lines.filter(|l| l.kind != LineKind::Context).collect();
    let count = |u| changed.iter().filter(|l| l.unchanged == u).count();
    (
        count(Unchanged::No),
        count(Unchanged::Moved),
        changed.iter().map(|l| l.emphasis.len()).sum(),
    )
}

/// The structural diff of each of those files: time, peak memory, regions,
/// and lines and emphasis against the line diff. Reported, never asserted,
/// except that the hunks still rebuild the new file.
#[test]
fn structural_research_files() {
    use dffy_core::structural;
    use std::time::Instant;

    let Some(dir) = bench_repo() else { return };
    // The syntax diff's parse budget is wall-clock, and a debug build misses it on the
    // 1.1MB generated table, so this only means something in release.
    if cfg!(debug_assertions) {
        eprintln!("skipping in a debug build; run `task test:kernel`");
        return;
    }
    let grammars = dffy_core::highlight::Registry::builtin();
    for path in RESEARCH_FILES {
        let fd = range_diff(&dir, path);
        let mut line = fd.hunks.clone();
        for hunk in &mut line {
            let emphasis = dffy_core::diff::emphasize(&fd.old, &fd.new, hunk);
            for (l, e) in hunk.lines.iter_mut().zip(emphasis) {
                l.emphasis = e;
            }
        }
        let mut times = Vec::new();
        let mut result = None;
        let mut memory = 0;
        for _ in 0..3 {
            reset_peak();
            let before = rss();
            let start = Instant::now();
            let s = structural::diff(&grammars, path.as_bytes(), &fd.old, &fd.new, &fd.hunks)
                .unwrap_or_else(|e| panic!("{path}: {}", e.note()));
            times.push(start.elapsed());
            memory = memory.max(peak().saturating_sub(before));
            result = Some(s);
        }
        let s = result.expect("ran");
        times.sort();
        assert_eq!(apply(&fd.old, &fd.new, &s.hunks), fd.new.bytes(), "{path}");
        // The disk cache: a miss computes and writes, a hit reads back.
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let cache = dffy_core::cache::Cache::new(cache_dir.path().to_owned(), 1 << 30);
        let ids = [
            fd.change.old.and_then(|s| s.id()),
            fd.change.new.and_then(|s| s.id()),
        ];
        let cached = || {
            let start = Instant::now();
            let s = structural::diff_cached(
                &grammars,
                path.as_bytes(),
                &fd.old,
                &fd.new,
                &fd.hunks,
                ids,
                Some(&cache),
            )
            .expect("diff");
            (start.elapsed(), s)
        };
        let (miss, _) = cached();
        let (hit, from_cache) = cached();
        assert_eq!(from_cache.hunks, s.hunks, "{path}");
        let (entry, _) = cache.usage().expect("usage");
        let r = s.regions;
        let (lc, _, le) = shown(&line);
        let (sc, sm, se) = shown(&s.hunks);
        eprintln!(
            "{path} ({} KB -> {} KB): median {:?} (min {:?}), peak +{} MB; regions: {} matched, \
             {} fell back, {} one-sided; changed lines shown {lc} -> {sc} ({sm} moved), \
             emphasized ranges {le} -> {se}; cache miss {miss:?}, hit {hit:?}, entry {} KB",
            fd.old.bytes().len() >> 10,
            fd.new.bytes().len() >> 10,
            times[1],
            times[0],
            memory >> 20,
            r.matched,
            r.fell_back,
            r.one_sided,
            entry >> 10,
        );
    }
}

#[test]
fn structural_hunks_reproduce_new_on_sampled_c_files() {
    let Some(dir) = bench_repo() else { return };
    // The syntax diff's parse budget is wall-clock, which a loaded debug build
    // can miss; `task test:kernel` runs this in release.
    if cfg!(debug_assertions) {
        eprintln!("skipping in a debug build; run `task test:kernel`");
        return;
    }
    let repo = Repo::discover(&dir).expect("repo");
    let spec = repo.resolve(&["v6.0", "v6.1"], false, false).expect("spec");
    let c_files: Vec<_> = collect(&repo, &spec)
        .into_iter()
        .filter(|c| {
            c.status == Status::Modified
                && (c.path.ends_with(b".c") || c.path.ends_with(b".h"))
                && !RESEARCH_FILES.iter().any(|p| c.path == *p)
        })
        .collect();
    let step = (c_files.len() / 120).max(1);
    let grammars = dffy_core::highlight::Registry::builtin();
    let (mut checked, mut regions, mut fell_back) = (0, 0, 0);
    for change in c_files.into_iter().step_by(step) {
        let path = change.path.to_string();
        let fd = repo
            .file_diff(change, &Options::default())
            .expect("file diff");
        if fd.binary || fd.too_large {
            continue;
        }
        let s =
            dffy_core::structural::diff(&grammars, path.as_bytes(), &fd.old, &fd.new, &fd.hunks)
                .unwrap_or_else(|e| panic!("{path}: {}", e.note()));
        assert_eq!(apply(&fd.old, &fd.new, &s.hunks), fd.new.bytes(), "{path}");
        checked += 1;
        regions += s.regions.matched + s.regions.fell_back;
        fell_back += s.regions.fell_back;
    }
    eprintln!(
        "structural: {checked} sampled C files rebuild; {fell_back} of {regions} regions fell back"
    );
    assert!(checked >= 50, "checked {checked}");
}

fn range_diff(dir: &Path, path: &str) -> dffy_core::git::FileDiff {
    let repo = Repo::discover(dir).expect("repo");
    let spec = repo.resolve(&["v6.0..v6.1"], false, false).expect("spec");
    let change = collect(&repo, &spec)
        .into_iter()
        .find(|c| c.path == path)
        .expect("changed");
    repo.file_diff(change, &Options::default()).expect("diff")
}

/// Highlighting costs on `kernel/sched/fair.c`, reported, never asserted:
/// the parse, the first screen (60 lines at the first hunk) and the whole
/// file, plus the disk cache's hit against its miss. Also checks that
/// screens highlighted alone equal the same lines of the whole file, when
/// the parse fits its time budget.
#[test]
fn highlight_timings() {
    use dffy_core::cache::Cache;
    use dffy_core::highlight::{self, Highlighter, Registry};
    use std::time::Instant;

    let Some(dir) = bench_repo() else { return };
    let grammars = Registry::builtin();
    let c = grammars.named("c").expect("C is bundled");
    // The parse budget is wall-clock, and a debug build under the load of
    // the whole suite runs past it, which leaves a side plain and stores
    // nothing. So the best of the runs that finished, if any did.
    let best = |f: &mut dyn FnMut() -> bool| {
        (0..5)
            .filter_map(|_| {
                let start = Instant::now();
                f().then(|| start.elapsed())
            })
            .min()
    };
    let shown = |time: Option<std::time::Duration>| {
        time.map_or_else(|| "over the parse budget".to_owned(), |t| format!("{t:?}"))
    };
    let fair = range_diff(&dir, "kernel/sched/fair.c");
    let text = &fair.new;
    let first = fair.hunks[0].new_start;
    let screen = first..(first + 60).min(text.len());
    // Before any other parse, so the allocator has no freed pages to reuse.
    let before = rss();
    let Some(parsed) = highlight::parse(c, text.bytes()) else {
        eprintln!("fair.c: the parse ran past its budget; run `task test:kernel`");
        return;
    };
    eprintln!(
        "fair.c tree: about {} bytes of memory per byte of source",
        rss().saturating_sub(before) / text.bytes().len()
    );
    let parse = best(&mut || highlight::parse(c, text.bytes()).is_some());
    let screens: Vec<usize> = fair.hunks.iter().step_by(7).map(|h| h.new_start).collect();
    let full = parsed.spans(text, 0..text.len());
    for &start in &screens {
        let range = start..(start + 60).min(text.len());
        assert_eq!(parsed.spans(text, range.clone()), full.slice(text, range));
    }
    let visible = best(&mut || {
        drop(parsed.spans(text, screen.clone()));
        true
    });
    let whole = best(&mut || {
        drop(parsed.spans(text, 0..text.len()));
        true
    });
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(cache_dir.path().to_owned(), 1 << 30);
    let id = fair.change.new.and_then(|s| s.id());
    let miss = best(&mut || {
        std::fs::remove_dir_all(cache_dir.path()).ok();
        Highlighter::prepare(c, text, id, Some(&cache), true).is_some()
    });
    // Without storing, so a missing entry (the last miss ran past the
    // budget) parses and doesn't count as a hit.
    let hit = best(
        &mut || match Highlighter::prepare(c, text, id, Some(&cache), false) {
            Some(h @ Highlighter::Cached(_)) => {
                drop(h.spans(text, screen.clone()));
                true
            }
            _ => false,
        },
    );
    let (bytes, _) = cache.usage().expect("usage");
    eprintln!(
        "fair.c ({} KB, {} lines): parse {}, first screen {}, whole file {}; \
         disk cache miss (parse + whole + write) {}, hit (read + slice) {}, entry {} KB",
        text.bytes().len() >> 10,
        text.len(),
        shown(parse),
        shown(visible),
        shown(whole),
        shown(miss),
        hit.map_or_else(|| "not measured".to_owned(), |t| format!("{t:?}")),
        bytes >> 10
    );
}

/// The clone's `drivers/net` (164MB, 7,000 files, read in place) against an
/// edited copy, with edits near both ends of the walk: the listing equals
/// `git diff --no-index --name-status --no-renames`, and the time to the
/// first change and to all of them (warm, best of five) is reported, never
/// asserted. It loads every core, so `.config/nextest.toml` keeps it apart
/// from the tests with wall-clock budgets.
#[test]
fn two_copies_of_a_kernel_folder_match_git_no_index() {
    use std::time::{Duration, Instant};

    use dffy_core::git::{Compare, Endpoint};

    let Some(dir) = bench_repo() else { return };
    let tmp = tempfile::tempdir().expect("tempdir");
    let (a, b) = (dir.join("drivers/net"), tmp.path().join("b"));
    let copied = Command::new("cp")
        .arg("-r")
        .arg(&a)
        .arg(&b)
        .status()
        .expect("cp runs");
    assert!(copied.success());
    let append = |path: &str| {
        let full = b.join(path);
        let mut text = std::fs::read(&full).expect("read");
        text.extend_from_slice(b"/* edited */\n");
        std::fs::write(&full, text).expect("write");
    };
    append("Kconfig");
    append("xen-netfront.c");
    std::fs::remove_file(b.join("ethernet/realtek/r8169_main.c")).expect("rm");
    std::fs::write(b.join("ethernet/added.c"), "int added;\n").expect("write");

    let repo = Repo::compare(Compare {
        old: Endpoint::Path(a),
        new: Endpoint::Path(b.clone()),
        name: "net".into(),
        exclude: Vec::new(),
        gitignore: false,
    })
    .expect("compare");
    let mut first = Vec::new();
    let mut all = Vec::new();
    let mut listed = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let mut first_at = None;
        listed.clear();
        repo.for_each_change(&DiffSpec::Paths, &[], |c| {
            first_at.get_or_insert_with(|| start.elapsed());
            listed.push(c);
            ControlFlow::Continue(())
        })
        .expect("listed");
        all.push(start.elapsed());
        first.push(first_at.unwrap_or(Duration::ZERO));
    }
    first.sort();
    all.sort();
    // Read-only; --no-index never touches the clone's index.
    let start = Instant::now();
    let out = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args([
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--name-status",
            "--no-renames",
            "-z",
            "drivers/net",
        ])
        .arg(&b)
        .output()
        .expect("git runs");
    let git_time = start.elapsed();
    let b_prefix = format!("{}/", b.display());
    let expected: Vec<(String, String)> = name_status(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .map(|(status, path)| {
            let rel = path
                .strip_prefix("drivers/net/")
                .or_else(|| path.strip_prefix(b_prefix.as_str()))
                .unwrap_or(&path);
            (status, rel.to_owned())
        })
        .collect();
    assert_eq!(ours(&listed), expected);
    eprintln!(
        "drivers/net, two copies: first change {:?}, all {} changes {:?} (best of 5); \
         git diff --no-index --name-status {git_time:?}",
        first[0],
        listed.len(),
        all[0],
    );
}

/// What the size cap avoids: tree-sitter on the largest changed file,
/// which dffy doesn't highlight.
#[test]
fn highlight_timings_over_the_cap() {
    use std::time::Instant;

    let Some(dir) = bench_repo() else { return };
    let table = range_diff(&dir, LARGEST);
    let text = &table.new;
    let language: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).expect("language");
    let start = Instant::now();
    let tree = parser.parse(text.bytes(), None).expect("parses");
    let parse = start.elapsed();
    let query = tree_sitter::Query::new(&language, tree_sitter_c::HIGHLIGHT_QUERY).expect("query");
    let count = |range: std::ops::Range<usize>| {
        use tree_sitter::StreamingIterator;
        let mut cursor = tree_sitter::QueryCursor::new();
        cursor.set_byte_range(range);
        let mut captures = cursor.captures(&query, tree.root_node(), text.bytes());
        let mut n = 0;
        while captures.next().is_some() {
            n += 1;
        }
        n
    };
    let first = table.hunks[0].new_start;
    let screen = text.offset(first)..text.offset((first + 60).min(text.len()));
    let start = Instant::now();
    count(screen);
    let visible = start.elapsed();
    let start = Instant::now();
    count(0..text.bytes().len());
    let whole = start.elapsed();
    eprintln!(
        "rtw8852c_table.c ({} KB, over the {} KB cap, not highlighted): parse {parse:?}, \
         first screen {visible:?}, whole file {whole:?}",
        text.bytes().len() >> 10,
        dffy_core::highlight::MAX_HIGHLIGHT_BYTES >> 10
    );
}
