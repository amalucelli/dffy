use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;

use super::*;
use crate::git::DiffSpec;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}");
}

fn repo(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q"]);
    write(dir.path(), files);
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "base"]);
    dir
}

fn write(dir: &Path, files: &[(&str, &[u8])]) {
    for (path, content) in files {
        std::fs::write(dir.join(path), content).expect("write");
    }
}

fn changes(repo: &Repo, spec: &DiffSpec) -> Vec<FileChange> {
    let mut out = Vec::new();
    repo.for_each_change(spec, &[], |c| {
        out.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    out
}

/// A store that reads from its first lookup and writes whatever it
/// counted, however quickly.
fn store(cache: &Cache) -> StatStore {
    let mut store = StatStore::new(cache.clone());
    store.worth_writing = Duration::ZERO;
    store.read_after = Duration::ZERO;
    store
}

fn cache() -> (tempfile::TempDir, Cache) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(dir.path().join("dffy"), 1 << 20);
    (dir, cache)
}

/// Counts kept in the disk cache are found by content: a store over the
/// same cache answers without the repository that has the blobs, but only
/// under the whitespace mode they were counted with, and only once it has
/// counted enough for reading to pay.
#[test]
fn stored_counts_are_found_by_content_and_whitespace_mode() {
    let dir = repo(&[("a.txt", b"one\ntwo\n")]);
    let d = dir.path();
    write(
        d,
        &[("a.txt", b"one\n two\nthree\n"), ("b.bin", b"\0binary\n")],
    );
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "edit"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo
        .resolve(&["HEAD~1", "HEAD"], false, false)
        .expect("spec");
    let changes = changes(&repo, &spec);
    let exact: Vec<_> = changes
        .iter()
        .map(|c| repo.stat(c, Whitespace::Exact).expect("stat"))
        .collect();
    assert_ne!(
        exact[0],
        repo.stat(&changes[0], Whitespace::IgnoreAll).expect("stat")
    );
    assert!(exact[1].binary.is_some());
    let (_dir, cache) = cache();
    let counting = store(&cache);
    for (change, stat) in changes.iter().zip(&exact) {
        assert_eq!(
            counting.stat(&repo, change, Whitespace::Exact).ok(),
            Some(*stat)
        );
    }
    drop(counting);

    let empty = tempfile::tempdir().expect("tempdir");
    git(empty.path(), &["init", "-q"]);
    let elsewhere = Repo::discover(empty.path()).expect("repo");
    assert!(
        StatStore::new(cache.clone())
            .stat(&elsewhere, &changes[0], Whitespace::Exact)
            .is_err(),
        "read before counting anything"
    );
    let found = store(&cache);
    for (change, stat) in changes.iter().zip(&exact) {
        assert_eq!(
            found.stat(&elsewhere, change, Whitespace::Exact).ok(),
            Some(*stat),
            "from the cache"
        );
    }
    assert!(
        found
            .stat(&elsewhere, &changes[0], Whitespace::IgnoreAll)
            .is_err(),
        "-w found the count of an exact comparison"
    );
}

/// A working-tree side is known by the id the status scan hashed, so an
/// edit that keeps the file's size is counted again rather than found.
#[test]
fn stored_counts_follow_working_tree_edits() {
    let dir = repo(&[("a.txt", b"a\nb\nc\n")]);
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    let spec = DiffSpec::IndexToWorktree { untracked: false };
    let (_dir, cache) = cache();
    let mut counted = Vec::new();
    for edit in [b"a\nX\nc\n", b"X\nY\nZ\n"] {
        write(d, &[("a.txt", edit)]);
        // An mtime the index doesn't have, so the scan hashes the file.
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(d.join("a.txt"))
            .and_then(|f| f.set_modified(old))
            .expect("set mtime");
        let change = &changes(&repo, &spec)[0];
        assert!(
            matches!(change.new, Some(Side::Worktree { id: Some(_), .. })),
            "the scan hashed it"
        );
        let stat = store(&cache)
            .stat(&repo, change, Whitespace::Exact)
            .expect("stat");
        counted.push((stat.added, stat.removed));
    }
    assert_eq!(counted, [(1, 1), (3, 3)]);
}

/// Counts that took little time to get aren't written, so a quick list
/// runs as fast as without the cache.
#[test]
fn quick_counts_are_not_written() {
    let dir = repo(&[("a.txt", b"one\n")]);
    let d = dir.path();
    write(d, &[("a.txt", b"two\n")]);
    git(d, &["commit", "-q", "-am", "edit"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo
        .resolve(&["HEAD~1", "HEAD"], false, false)
        .expect("spec");
    let change = &changes(&repo, &spec)[0];
    let (_dir, cache) = cache();
    StatStore::new(cache.clone())
        .stat(&repo, change, Whitespace::Exact)
        .expect("stat");
    assert_eq!(
        cache.usage().expect("usage").1,
        0,
        "a quick count was written"
    );
    store(&cache)
        .stat(&repo, change, Whitespace::Exact)
        .expect("stat");
    assert_eq!(cache.usage().expect("usage").1, 1, "the change can be kept");
}

/// A list whose counts were never kept stops reading shards after a few
/// lookups find nothing.
#[test]
fn reading_stops_when_nothing_is_found() {
    let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
    let side = |word: &str| -> Vec<(&str, Vec<u8>)> {
        names
            .iter()
            .map(|n| (*n, format!("{word} {n}\n").into_bytes()))
            .collect()
    };
    let (old, new) = (side("one"), side("two"));
    let dir = repo(&old.iter().map(|(n, c)| (*n, &c[..])).collect::<Vec<_>>());
    let d = dir.path();
    write(
        d,
        &new.iter().map(|(n, c)| (*n, &c[..])).collect::<Vec<_>>(),
    );
    git(d, &["commit", "-q", "-am", "edit"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo
        .resolve(&["HEAD~1", "HEAD"], false, false)
        .expect("spec");
    let (_dir, cache) = cache();
    let store = store(&cache);
    for change in changes(&repo, &spec) {
        store.stat(&repo, &change, Whitespace::Exact).expect("stat");
    }
    let read = (0..store.shards.len())
        .filter(|&slot| store.lock(slot).read)
        .count();
    assert_eq!(read, MISSES as usize);
}
