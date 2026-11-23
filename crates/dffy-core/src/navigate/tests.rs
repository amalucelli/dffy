use super::*;

fn paths(text: &str) -> Vec<String> {
    uses(text.as_bytes())
        .iter()
        .map(|p| {
            p.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("::")
        })
        .collect()
}

#[test]
fn use_trees_expand_to_the_paths_they_name() {
    let text = "use std::sync::Arc;\n\
                pub(crate) use crate::git::{self, Repo as R, tree::{Walk, *}};\n\
                // the cache we use here;\n\
                fn user() {}\n\
                use super::*;\n";
    assert_eq!(
        paths(text),
        [
            "std::sync::Arc",
            "crate::git",
            "crate::git::Repo",
            "crate::git::tree::Walk",
            "crate::git::tree",
            // Comments aren't told apart; such paths name no file.
            "here",
            "super",
        ]
    );
}

#[test]
fn includes_are_read_from_either_form() {
    let text =
        b"#include <linux/slab.h>\n  #  include \"local.h\"\n#define include x\n#include MACRO\n";
    let found: Vec<_> = includes(text).collect();
    assert_eq!(found, [&b"linux/slab.h"[..], b"local.h"]);
}

#[test]
fn joined_paths_work_out_dots_and_stay_in_the_root() {
    assert_eq!(
        joined(b"fs/ext4/".as_bstr(), b"../internal.h"),
        Some("fs/internal.h".into())
    );
    assert_eq!(joined(b"".as_bstr(), b"../x.h"), None);
}

/// The diff between a spec's snapshots, either way round, runs between
/// those snapshots in that order; a tree against the working tree lists
/// what the uncommitted diff does.
#[test]
fn the_diff_between_snapshots_is_the_spec_they_came_from() {
    let (a, b) = (
        ObjectId::null(gix::hash::Kind::Sha1),
        ObjectId::empty_tree(gix::hash::Kind::Sha1),
    );
    for spec in [
        git::DiffSpec::TreeToTree { old: a, new: b },
        git::DiffSpec::TreeToIndex { tree: a },
        git::DiffSpec::Uncommitted { head: a },
        git::DiffSpec::IndexToWorktree { untracked: true },
    ] {
        for reverse in [false, true] {
            let snapshots = spec.snapshots(reverse).expect("snapshots");
            let (found, reversed) = git::DiffSpec::between(snapshots).expect("a diff");
            assert_eq!(
                found.snapshots(reversed),
                Some(snapshots),
                "{spec:?} {reverse}"
            );
        }
    }
    let checkpoint = git::DiffSpec::Checkpoint { tree: a }.snapshots(false);
    assert_eq!(
        git::DiffSpec::between(checkpoint.expect("snapshots")),
        Some((git::DiffSpec::Uncommitted { head: a }, false))
    );
    assert_eq!(git::DiffSpec::between([Snapshot::Worktree; 2]), None);
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}");
}

/// A chunk stopped part way gives back the files it didn't reach, and they
/// are taken again next.
#[test]
fn a_chunk_stopped_part_way_gives_back_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let names = ["a.c", "b.c", "c.c"];
    for name in names {
        std::fs::write(d.join(name), "int f(void);\n").expect("write");
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    for name in names {
        std::fs::write(d.join(name), "int g(void)\n{\n\treturn 0;\n}\n").expect("write");
    }
    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], false, false).expect("resolves");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        std::ops::ControlFlow::Continue(())
    })
    .expect("enumerates");
    let diff = Diff {
        snapshots: spec.snapshots(false).expect("snapshots"),
        changes: &changes,
        options: Options::default(),
    };
    let grammars = Registry::builtin();
    let nav =
        Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New).expect("lists");
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = crate::cache::Cache::new(cache_dir.path().to_owned(), 1 << 20).symbols();

    let chunk = nav.take_chunk().expect("the working tree's files");
    assert_eq!(chunk.len(), 3);
    let mut asked = 0;
    let done = chunk.run_until(&repo, &cache, || {
        asked += 1;
        asked > 1
    });
    assert!(nav.add(done).is_none());
    assert_eq!(nav.progress(), Progress { done: 1, total: 3 });
    assert_eq!(nav.take_chunk().map(|c| c.len()), Some(2));
}

/// A name's uses: closest files first, past comments, strings and, once
/// indexed, the lines defining it; a search stopped part way goes on where
/// it stopped, and past the first 200 files with a use it only counts.
#[test]
fn references_list_the_uses_closest_first_and_count_past_the_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(d.join("lib")).expect("mkdir");
    std::fs::create_dir_all(d.join("far")).expect("mkdir");
    std::fs::write(d.join("lib/f.h"), "int frob(int x)\n{\n\treturn x;\n}\n").expect("write");
    std::fs::write(
        d.join("lib/use.c"),
        "#include \"f.h\"\n// frob it\nint a = frob(1); const char *s = \"frob\";\nint b = frob(2) + frob(3);\n",
    )
    .expect("write");
    std::fs::write(d.join("lib/notes.md"), "Call frob when ready.\n").expect("write");
    for i in 0..205 {
        std::fs::write(d.join(format!("far/u{i:03}.c")), "int u = frob(0);\n").expect("write");
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], false, false).expect("resolves");
    let diff = Diff {
        snapshots: spec.snapshots(false).expect("snapshots"),
        changes: &[],
        options: Options::default(),
    };
    let grammars = Registry::builtin();
    let nav =
        Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New).expect("lists");
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = crate::cache::Cache::new(cache_dir.path().to_owned(), 1 << 22).symbols();
    nav.build(&repo, &cache, 1, &AtomicBool::new(false), |_| {})
        .expect("builds");
    let origin = Origin {
        path: "lib/use.c".into(),
        language: grammars.named("c").expect("bundled").clone(),
    };
    let go = AtomicBool::new(false);
    let mut search = nav
        .references(&repo, "frob", &origin, Filter::default(), &go)
        .expect("starts");
    let mut asked = 0;
    let first = search.step(&nav, &repo, || {
        asked += 1;
        asked > 2
    });
    let at = |hits: &[Reference]| -> Vec<String> {
        hits.iter()
            .map(|r| format!("{}:{}", r.candidate.path, r.candidate.start + 1))
            .collect()
    };
    assert_eq!(
        at(&first),
        ["lib/use.c:3", "lib/use.c:4"],
        "the asking file first, each line once"
    );
    assert_eq!(first[0].text, "int a = frob(1); const char *s = \"frob\";");
    assert!(!search.done());
    let rest = search.step(&nav, &repo, || false);
    assert!(search.done());
    let rest = at(&rest);
    assert!(
        !rest.iter().any(|r| r.starts_with("lib/f.h")),
        "its definition: {rest:?}"
    );
    assert!(
        rest.contains(&"lib/notes.md:1".to_owned()),
        "any file's words count: {rest:?}"
    );
    assert_eq!(search.files(), 1 + 1 + 205);
    let listed: std::collections::BTreeSet<&str> = rest
        .iter()
        .map(|r| r.split(':').next().unwrap_or_default())
        .collect();
    assert_eq!(
        listed.len() + 1,
        MAX_REFERENCE_FILES,
        "uses of the first 200 files only"
    );
}

/// The diff of the index with the working tree of the repository at `d`.
fn worktree_diff(d: &std::path::Path) -> (Repo, git::DiffSpec, Vec<FileChange>) {
    let repo = Repo::discover(d).expect("repo");
    let unstaged = git::Revisions {
        unstaged: true,
        ..git::Revisions::default()
    };
    let spec = repo.resolve_revisions(&unstaged).expect("resolves");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        std::ops::ControlFlow::Continue(())
    })
    .expect("enumerates");
    (repo, spec, changes)
}

/// The definition naming the name is left out even when it names it on a
/// later line than it starts on, a use in the body counts; a doc comment
/// over several lines doesn't; past 1000 lines a file's uses are counted;
/// a search stepped with another navigator finds nothing; an empty name
/// finds nothing.
#[test]
fn references_skip_multiline_definitions_and_block_comments_and_cap_a_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::write(
        d.join("a.c"),
        "/**\n * frob - frobs\n * frob(x) again\n */\nstatic inline int\nfrob(int x)\n{\n\treturn x ? frob(x - 1) : 0;\n}\nint a = frob(1); /* frob\n frob */ int b = frob(2);\n",
    )
    .expect("write");
    std::fs::write(d.join("many.c"), "int v = frob(1);\n".repeat(1005)).expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    let (repo, spec, changes) = worktree_diff(d);
    let diff = Diff {
        snapshots: spec.snapshots(false).expect("snapshots"),
        changes: &changes,
        options: Options::default(),
    };
    let grammars = Registry::builtin();
    let nav =
        Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New).expect("lists");
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = crate::cache::Cache::new(cache_dir.path().to_owned(), 1 << 22).symbols();
    nav.build(&repo, &cache, 1, &AtomicBool::new(false), |_| {})
        .expect("builds");
    let origin = Origin {
        path: "a.c".into(),
        language: grammars.named("c").expect("bundled").clone(),
    };
    let go = AtomicBool::new(false);
    let mut search = nav
        .references(&repo, "frob", &origin, Filter::default(), &go)
        .expect("starts");
    let hits = search.step(&nav, &repo, || false);
    let lines: Vec<String> = (hits.iter())
        .filter(|r| r.candidate.path == "a.c")
        .map(|r| format!("{}", r.candidate.start + 1))
        .collect();
    assert_eq!(lines, ["8", "10", "11"]);
    let many: Vec<&Reference> = hits
        .iter()
        .filter(|r| r.candidate.path == "many.c")
        .collect();
    assert_eq!(many.len(), MAX_REFERENCE_LINES);
    assert_eq!(many.last().map(|r| r.more), Some(5));

    let other =
        Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New).expect("lists");
    let mut search = nav
        .references(&repo, "frob", &origin, Filter::default(), &go)
        .expect("starts");
    assert_eq!(search.step(&other, &repo, || false), []);
    assert!(search.done(), "another navigator's search ends");
    let mut empty = nav
        .references(&repo, "", &origin, Filter::default(), &go)
        .expect("starts");
    assert!(empty.done() && empty.step(&nav, &repo, || false).is_empty());
}

/// Whether a navigator still answers for a diff read again is the core's
/// call: the same diff is the same; a working tree whose modified file was
/// edited again is a new one; the index's side only takes the changes again
/// until the index itself moves.
#[test]
fn a_navigator_fits_the_diff_it_was_made_for() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join("a.c"), "int f(void);\n").expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    std::fs::write(d.join("a.c"), "int g(void)\n{\n\treturn 0;\n}\n").expect("write");
    let (repo, spec, changes) = worktree_diff(d);
    let snapshots = spec.snapshots(false).expect("snapshots");
    let diff = Diff {
        snapshots,
        changes: &changes,
        options: Options::default(),
    };
    let grammars = Registry::builtin();
    let shared = Shared::new();
    let new = Navigator::new(&repo, &grammars, &shared, &diff, view::Side::New).expect("lists");
    let old = Navigator::new(&repo, &grammars, &shared, &diff, view::Side::Old).expect("lists");
    assert_eq!(new.refit(&repo, &diff), Fit::Same);
    // A size the first edit didn't have, so the stat tells them apart.
    std::fs::write(d.join("a.c"), "int g(void)\n{\n\treturn 1 + 1;\n}\n").expect("write");
    let (_, _, again) = worktree_diff(d);
    let edited = Diff {
        snapshots,
        changes: &again,
        options: Options::default(),
    };
    assert_eq!(new.refit(&repo, &edited), Fit::New);
    assert_eq!(
        old.refit(&repo, &edited),
        Fit::Changes,
        "the index didn't move"
    );
    assert_eq!(old.refit(&repo, &edited), Fit::Same);
    git(d, &["add", "a.c"]);
    let (_, _, staged) = worktree_diff(d);
    let staged = Diff {
        snapshots,
        changes: &staged,
        options: Options::default(),
    };
    assert_eq!(old.refit(&repo, &staged), Fit::New, "the index moved");
}

/// The asking file's own definitions never wait for the index build: on a
/// cold navigator, with the local tier's time used up before it starts, a
/// lookup still reads the asking file and finds the name it defines.
#[test]
fn a_cold_lookup_finds_what_the_asking_file_defines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::write(
        d.join("a.c"),
        "#include \"b.h\"\nstatic int frob(int x)\n{\n\treturn x;\n}\nint use(void) { return frob(1); }\n",
    )
    .expect("write");
    std::fs::write(d.join("b.h"), "int other(void);\n").expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], false, false).expect("resolves");
    let diff = Diff {
        snapshots: spec.snapshots(false).expect("snapshots"),
        changes: &[],
        options: Options::default(),
    };
    let grammars = Registry::builtin();
    let nav = Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New)
        .expect("lists")
        .with_local_budget(Duration::ZERO);
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = crate::cache::Cache::new(cache_dir.path().to_owned(), 1 << 20).symbols();
    let origin = Origin {
        path: "a.c".into(),
        language: grammars.named("c").expect("bundled").clone(),
    };
    let found = nav
        .lookup(&repo, &cache, "frob", &origin, &AtomicBool::new(false))
        .expect("looks up");
    let at: Vec<String> = (found.candidates.iter())
        .map(|c| format!("{}:{}", c.path, c.start + 1))
        .collect();
    assert_eq!(at, ["a.c:2"]);
}

/// Two commits of a small C project, `src/use.c` changed in the second,
/// and the diff between them: `frob` is defined in `src/b.c`.
fn stored_project() -> (tempfile::TempDir, Repo, Vec<FileChange>, [Snapshot; 2]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(d.join("src")).expect("mkdir");
    std::fs::write(d.join("src/a.h"), "struct frobber { int n; };\n").expect("write");
    std::fs::write(d.join("src/b.c"), "int frob(int x)\n{\n\treturn x;\n}\n").expect("write");
    std::fs::write(d.join("src/use.c"), "int use(void) { return frob(1); }\n").expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "first"]);
    std::fs::write(d.join("src/use.c"), "int use(void) { return frob(2); }\n").expect("write");
    git(d, &["commit", "-q", "-am", "second"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo
        .resolve(&["HEAD~1", "HEAD"], false, false)
        .expect("resolves");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        std::ops::ControlFlow::Continue(())
    })
    .expect("enumerates");
    let snapshots = spec.snapshots(false).expect("snapshots");
    (dir, repo, changes, snapshots)
}

fn built(
    repo: &Repo,
    changes: &[FileChange],
    snapshots: [Snapshot; 2],
    (shared, cache): (&Shared, &SymbolCache),
    side: view::Side,
) -> Navigator {
    let diff = Diff {
        snapshots,
        changes,
        options: Options::default(),
    };
    let nav = Navigator::new(repo, &Registry::builtin(), shared, &diff, side).expect("lists");
    nav.build(repo, cache, 1, &AtomicBool::new(false), |_| {})
        .expect("the queries compile");
    nav
}

/// Where `frob` is defined, asked from `src/use.c`.
fn frob(nav: &Navigator, repo: &Repo, cache: &SymbolCache) -> Vec<String> {
    let origin = Origin {
        path: "src/use.c".into(),
        language: Registry::builtin().named("c").expect("bundled").clone(),
    };
    let found =
        (nav.lookup(repo, cache, "frob", &origin, &AtomicBool::new(false))).expect("looks up");
    (found.candidates.iter())
        .map(|c| format!("{}:{}", c.path, c.start))
        .collect()
}

/// A built index answers from the store's file and its postings there,
/// which nothing in the cache's folder names: the symbol cache gone, its
/// packs evicted, the answers stay.
#[test]
fn a_built_index_answers_from_the_store_with_the_cache_gone() {
    let (_dir, repo, changes, snapshots) = stored_project();
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = crate::cache::Cache::new(cache_dir.path().to_owned(), 1 << 24).symbols();
    let shared = Shared::new();
    let nav = built(
        &repo,
        &changes,
        snapshots,
        (&shared, &cache),
        view::Side::New,
    );
    assert!(nav.postings_written());
    assert!(shared.stored_bytes() > 0);
    assert_eq!(frob(&nav, &repo, &cache), ["src/b.c:0"]);
    let left: Vec<String> = std::fs::read_dir(cache.as_cache().dir())
        .expect("the cache's folder")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("navigate"))
        .collect();
    assert!(left.is_empty(), "the store's file is unlinked: {left:?}");

    std::fs::remove_dir_all(cache_dir.path()).expect("removes");
    assert_eq!(frob(&nav, &repo, &cache), ["src/b.c:0"]);
    assert_eq!(nav.note(), None);
}

/// Both versions of a diff share the store: a blob they both have is
/// stored once, by whichever navigator read it first.
#[test]
fn both_versions_store_an_unchanged_blob_once() {
    let (_dir, repo, changes, snapshots) = stored_project();
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = crate::cache::Cache::new(cache_dir.path().to_owned(), 1 << 24).symbols();
    let shared = Shared::new();
    let old = built(
        &repo,
        &changes,
        snapshots,
        (&shared, &cache),
        view::Side::Old,
    );
    let after_old = shared.stored_bytes();
    let new = built(
        &repo,
        &changes,
        snapshots,
        (&shared, &cache),
        view::Side::New,
    );
    assert_eq!(new.stats().cached, 2, "a.h and b.c from the store");
    assert!(
        shared.stored_bytes() - after_old < after_old,
        "only use.c's, and the postings, added"
    );
    assert_eq!(frob(&old, &repo, &cache), frob(&new, &repo, &cache));
}

/// A store whose file can't be read back whole takes the index's files out
/// to be read again: a lookup then answers once they are.
#[test]
fn a_store_read_that_fails_reads_the_index_again() {
    let (_dir, repo, changes, snapshots) = stored_project();
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = crate::cache::Cache::new(cache_dir.path().to_owned(), 1 << 24).symbols();
    let shared = Shared::new();
    let nav = built(
        &repo,
        &changes,
        snapshots,
        (&shared, &cache),
        view::Side::New,
    );
    nav.shared.store().damage();
    frob(&nav, &repo, &cache);
    assert!(!nav.progress().complete(), "taken out to be read again");
    nav.build(&repo, &cache, 1, &AtomicBool::new(false), |_| {})
        .expect("the queries compile");
    assert!(nav.progress().complete());
    assert_eq!(frob(&nav, &repo, &cache), ["src/b.c:0"]);
}

/// Where the store's file can't be made, as on a full disk, the
/// definitions are kept in memory and the navigator says so.
#[test]
fn a_store_that_cant_be_made_keeps_definitions_in_memory() {
    let (_dir, repo, changes, snapshots) = stored_project();
    let blocked = tempfile::NamedTempFile::new().expect("a file");
    let cache = crate::cache::Cache::new(blocked.path().to_owned(), 1 << 24).symbols();
    let shared = Shared::new();
    let nav = built(
        &repo,
        &changes,
        snapshots,
        (&shared, &cache),
        view::Side::New,
    );
    assert_eq!(frob(&nav, &repo, &cache), ["src/b.c:0"]);
    let note = nav.note().expect("said");
    assert!(note.contains("kept in memory"), "{note}");
}

/// Navigators made again and dropped, as a watched working tree's are,
/// don't grow the store: one made when the file holds more dead than live
/// starts another. One still held keeps its file, and its answers.
#[test]
fn navigators_made_again_keep_the_store_bounded() {
    let (_dir, repo, changes, snapshots) = stored_project();
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache = crate::cache::Cache::new(cache_dir.path().to_owned(), 1 << 24).symbols();
    let shared = Shared::new();
    let kept = built(
        &repo,
        &changes,
        snapshots,
        (&shared, &cache),
        view::Side::Old,
    );
    let one = shared.stored_bytes();
    for _ in 0..8 {
        let nav = built(
            &repo,
            &changes,
            snapshots,
            (&shared, &cache),
            view::Side::New,
        );
        assert_eq!(frob(&nav, &repo, &cache), ["src/b.c:0"]);
    }
    assert!(
        shared.stored_bytes() <= 3 * one,
        "{} bytes stored, one navigator's {one}",
        shared.stored_bytes()
    );
    assert_eq!(frob(&kept, &repo, &cache), ["src/b.c:0"]);
}

/// A use calls the name when its arguments follow, past a Rust macro's
/// `!` or type arguments; a definition's own name doesn't.
#[test]
fn a_call_is_the_name_before_its_arguments() {
    let call = |line: &str, name: &str| {
        let at = line.find(name).expect("the name");
        let bytes = line.as_bytes();
        references::calls(&bytes[..at], &bytes[at + name.len()..])
    };
    assert!(call("x = frob(1);", "frob"));
    assert!(call("\treturn frob (1);", "frob"));
    assert!(call("let v = vec![1];", "vec"));
    assert!(call("s.parse::<u32>()", "parse"));
    assert!(call("make<Item>(x)", "make"));
    assert!(call("Map[int](x)", "Map"));
    assert!(!call("let f = frob;", "frob"));
    assert!(!call("fn frob(x: u32) {", "frob"));
    assert!(!call("def frob(x):", "frob"));
    assert!(!call("if a < frob {", "frob"));
}

/// The uses a filter keeps: those calling the name, those in the files
/// the diff changes.
#[test]
fn filters_keep_the_calls_and_the_uses_in_the_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let user = |n: u32| format!("int use(void) {{ return frob({n}); }}\nint (*f)(int) = frob;\n");
    std::fs::write(d.join("a.c"), user(1)).expect("write");
    std::fs::write(d.join("b.c"), user(1)).expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "first"]);
    std::fs::write(d.join("b.c"), user(2)).expect("write");
    git(d, &["commit", "-q", "-am", "second"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo
        .resolve(&["HEAD~1", "HEAD"], false, false)
        .expect("resolves");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        std::ops::ControlFlow::Continue(())
    })
    .expect("enumerates");
    let diff = Diff {
        snapshots: spec.snapshots(false).expect("snapshots"),
        changes: &changes,
        options: Options::default(),
    };
    let grammars = Registry::builtin();
    let nav =
        Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New).expect("lists");
    let origin = Origin {
        path: "b.c".into(),
        language: grammars.named("c").expect("bundled").clone(),
    };
    let go = AtomicBool::new(false);
    let kept = |filter| {
        let mut search = nav
            .references(&repo, "frob", &origin, filter, &go)
            .expect("starts");
        let mut found: Vec<String> = Vec::new();
        while !search.done() {
            found.extend(
                (search.step(&nav, &repo, || false).iter())
                    .map(|r| format!("{}:{}", r.candidate.path, r.candidate.start + 1)),
            );
        }
        found.sort();
        found
    };
    assert_eq!(
        kept(Filter::default()),
        ["a.c:1", "a.c:2", "b.c:1", "b.c:2"]
    );
    let calls = Filter {
        calls_only: true,
        ..Filter::default()
    };
    assert_eq!(kept(calls), ["a.c:1", "b.c:1"]);
    let in_diff = Filter {
        in_diff: true,
        ..Filter::default()
    };
    assert_eq!(kept(in_diff), ["b.c:1", "b.c:2"]);
}
