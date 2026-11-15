//! Rename detection checked against `git diff -M --name-status` on fixture
//! repositories: the same pairs, in the same places, with the same scores.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;

use dffy_core::cache::Cache;
use dffy_core::git::{FileChange, Repo, Status};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 output")
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

/// Lines numbered over `range` and tagged `tag`, for content whose
/// similarity to another file is easy to set: files sharing k of n lines
/// share about k/n of their bytes.
fn lines(tag: &str, range: std::ops::Range<usize>) -> String {
    range.fold(String::new(), |mut out, i| {
        let _ = writeln!(out, "{tag} line number {i:04}");
        out
    })
}

/// A repository with a base commit, then `change` applied and committed.
fn repo(base: &[(&str, String)], change: impl FnOnce(&Path)) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    for (path, content) in base {
        write(d, path, content);
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    change(d);
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "change"]);
    dir
}

/// git's view as `(status, paths)` lines.
fn expected(dir: &Path, args: &[&str]) -> Vec<String> {
    let mut full = vec!["diff", "--no-ext-diff", "-M", "--name-status", "-z"];
    full.extend_from_slice(args);
    let out = git(dir, &full);
    let mut fields = out.split('\0').filter(|s| !s.is_empty());
    let mut rows = Vec::new();
    while let Some(status) = fields.next() {
        let paths = if status.starts_with('R') || status.starts_with('C') {
            format!(
                "{}\t{}",
                fields.next().expect("old"),
                fields.next().expect("new")
            )
        } else {
            fields.next().expect("path").to_owned()
        };
        rows.push(format!("{status}\t{paths}"));
    }
    rows
}

fn ours(dir: &Path, revs: &[&str], cache: Option<&Cache>) -> Vec<String> {
    let repo = Repo::discover(dir).expect("repo");
    let spec = repo.resolve(revs, false, false).expect("spec");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let changes = repo.detect_renames(changes, cache).expect("renames");
    changes.iter().map(row).collect()
}

fn row(c: &FileChange) -> String {
    let letter = match &c.status {
        Status::Renamed { from, similarity } => {
            return format!("R{similarity:03}\t{from}\t{}", c.path);
        }
        Status::Added => "A",
        Status::Deleted => "D",
        Status::Modified => "M",
        Status::TypeChange => "T",
        Status::Untracked => "?",
    };
    format!("{letter}\t{}", c.path)
}

fn check(dir: &Path) {
    assert_eq!(
        ours(dir, &["HEAD~1", "HEAD"], None),
        expected(dir, &["HEAD~1", "HEAD"])
    );
}

#[test]
fn exact_renames_prefer_an_unused_source_with_the_same_name() {
    let same = lines("same", 0..20);
    let dir = repo(
        &[
            ("a/one.txt", same.clone()),
            ("b/two.txt", same.clone()),
            ("c/keep.txt", lines("keep", 0..5)),
            ("empty", String::new()),
        ],
        |d| {
            for p in ["a/one.txt", "b/two.txt", "empty"] {
                std::fs::remove_file(d.join(p)).expect("rm");
            }
            write(d, "x/two.txt", &same);
            write(d, "x/three.txt", &same);
            write(d, "y/empty", "");
        },
    );
    check(dir.path());
    let rows = ours(dir.path(), &["HEAD~1", "HEAD"], None);
    assert!(
        rows.contains(&"R100\tb/two.txt\tx/two.txt".to_owned()),
        "{rows:?}"
    );
}

#[test]
fn similar_files_pair_and_dissimilar_ones_stay_apart() {
    let dir = repo(
        &[
            ("src/old_name.c", lines("a", 0..40)),
            ("src/unrelated.c", lines("b", 0..40)),
        ],
        |d| {
            std::fs::remove_file(d.join("src/old_name.c")).expect("rm");
            std::fs::remove_file(d.join("src/unrelated.c")).expect("rm");
            // 30 of 40 lines kept: about 75% similar.
            write(
                d,
                "lib/new_name.c",
                &(lines("a", 0..30) + &lines("z", 0..10)),
            );
            // 15 of 40 kept: under the 50% threshold.
            write(
                d,
                "lib/rewritten.c",
                &(lines("b", 0..15) + &lines("y", 0..25)),
            );
        },
    );
    check(dir.path());
    let rows = ours(dir.path(), &["HEAD~1", "HEAD"], None);
    assert!(rows.iter().any(|r| r.starts_with("R07")), "{rows:?}");
    assert!(rows.contains(&"D\tsrc/unrelated.c".to_owned()), "{rows:?}");
}

#[test]
fn file_name_matches_need_a_higher_score_and_win_ties() {
    let base = lines("shared", 0..40);
    let dir = repo(
        &[
            ("old/util.c", base.clone()),
            ("old/helper.c", base + &lines("h", 0..1)),
            ("old/name.c", lines("n", 0..40)),
        ],
        |d| {
            for p in ["old/util.c", "old/helper.c", "old/name.c"] {
                std::fs::remove_file(d.join(p)).expect("rm");
            }
            // Equally similar to util.c and helper.c; the name decides.
            write(
                d,
                "new/util.c",
                &(lines("shared", 0..36) + &lines("u", 0..4)),
            );
            // Same name, but only about 60% similar: too little for the
            // name pass, enough for the matrix.
            write(d, "new/name.c", &(lines("n", 0..24) + &lines("m", 0..16)));
        },
    );
    check(dir.path());
}

/// Many deletions and additions sharing chunks in a tangle, so the
/// best-four candidates, the greedy order and the tie-breaks all matter.
#[test]
fn a_tangle_of_candidates_pairs_like_git() {
    let mut seed = 0x2545_f491_u64;
    let mut next = move |n: usize| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        usize::try_from(seed >> 33).expect("31 bits") % n
    };
    let pool: Vec<String> = (0..12).map(|i| lines(&format!("blk{i}"), 0..8)).collect();
    let mut base = Vec::new();
    for i in 0..24 {
        let content: String = (0..5).map(|_| pool[next(12)].clone()).collect();
        base.push((format!("old/dir{}/f{i}.txt", i % 3), content));
    }
    let base_ref: Vec<(&str, String)> = base.iter().map(|(p, c)| (p.as_str(), c.clone())).collect();
    let dir = repo(&base_ref, |d| {
        for (path, _) in &base {
            std::fs::remove_file(d.join(path)).expect("rm");
        }
        for i in 0..24 {
            let content: String = (0..5).map(|_| pool[next(12)].clone()).collect();
            let name = if i % 4 == 0 {
                format!("f{i}.txt")
            } else {
                format!("g{i}.txt")
            };
            write(d, &format!("new/dir{}/{name}", i % 2), &content);
        }
    });
    check(dir.path());
    let rows = ours(dir.path(), &["HEAD~1", "HEAD"], None);
    let inexact = rows
        .iter()
        .filter(|r| r.starts_with('R') && !r.starts_with("R100"))
        .count();
    assert!(inexact >= 5, "{rows:#?}");
}

#[test]
fn staged_renames_and_reversed_diffs() {
    let dir = repo(&[("a.txt", lines("a", 0..30))], |d| {
        std::fs::remove_file(d.join("a.txt")).expect("rm");
        write(d, "b.txt", &(lines("a", 0..28) + "tail\n"));
    });
    let d = dir.path();
    check(d);
    git(d, &["mv", "b.txt", "c.txt"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], true, false).expect("staged spec");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c.reversed());
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let rows: Vec<String> = repo
        .detect_renames(changes, None)
        .expect("renames")
        .iter()
        .map(row)
        .collect();
    assert_eq!(rows, expected(d, &["--cached", "-R"]));
}

#[test]
fn cached_results_match_fresh_ones() {
    let dir = repo(
        &[("a/x.rs", lines("x", 0..30)), ("a/y.rs", lines("y", 0..30))],
        |d| {
            std::fs::remove_file(d.join("a/x.rs")).expect("rm");
            std::fs::remove_file(d.join("a/y.rs")).expect("rm");
            write(d, "b/x.rs", &lines("x", 0..25));
            write(d, "b/z.rs", &lines("y", 0..30));
        },
    );
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(cache_dir.path().to_owned(), 1 << 20);
    let fresh = ours(dir.path(), &["HEAD~1", "HEAD"], None);
    assert_eq!(ours(dir.path(), &["HEAD~1", "HEAD"], Some(&cache)), fresh);
    assert_eq!(cache.usage().expect("usage").1, 1);
    assert_eq!(ours(dir.path(), &["HEAD~1", "HEAD"], Some(&cache)), fresh);
}
