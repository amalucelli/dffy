//! Go-to-definition on small fixture repositories: what a lookup finds at
//! each side of a diff, in what order, and what the index stores.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use dffy_core::cache::{Cache, SymbolCache};
use dffy_core::diff::Options;
use dffy_core::git::{DiffSpec, FileChange, Repo, Side};
use dffy_core::highlight::{self, Registry};
use dffy_core::navigate::{Diff, Error, Lookup, Navigator, Origin, Rank, Shared};
use dffy_core::view;

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

const SLAB_H: &str = "#ifndef SLAB_H
#define SLAB_H
#include <linux/types.h>
static inline void *kmalloc_array(unsigned long n, unsigned long size)
{
\treturn 0;
}
#endif
";

const TYPES_H: &str = "struct list_head {
\tstruct list_head *next, *prev;
};
";

const SLAB_COMMON: &str = "#include <linux/slab.h>
#include \"slab.h\"

static int helper(void)
{
\treturn 1;
}

void *use(void)
{
\treturn kmalloc_array(helper(), SLAB_UNDEF);
}
";

const OTHER_C: &str = "int helper(void)
{
\treturn 2;
}
";

const SHAPE_RS: &str = "pub struct Point;

impl Point {
    pub fn len(&self) -> usize {
        0
    }
}
";

const OTHER_RS: &str = "use crate::shape::Point;

pub struct Line;

impl Line {
    pub fn len(&self) -> usize {
        1
    }
}

fn measure(p: Point) -> usize {
    p.len()
}
";

/// Kernel-like C, with copies under `tools/` and per-architecture headers
/// as the kernel has, and a Rust workspace of two crates; one commit, then
/// a second that edits `mm/other.c`'s `helper`, appends to
/// `mm/slab_common.c` and edits a Rust file.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    for (path, content) in [
        ("include/linux/slab.h", SLAB_H),
        ("include/linux/types.h", TYPES_H),
        ("tools/include/linux/slab.h", SLAB_H),
        ("tools/include/linux/types.h", TYPES_H),
        ("tools/lib/slab.c", TOOLS_SLAB),
        ("mm/slab_common.c", SLAB_COMMON),
        ("mm/slab.h", "#define SLAB_FLAG 1\n"),
        ("mm/other.c", OTHER_C),
        ("drivers/x.c", "int helper(void)\n{\n\treturn 3;\n}\n"),
        ("arch/x86/include/asm/page.h", "#define PAGE_SHIFT 12\n"),
        ("arch/arm/include/asm/page.h", "#define PAGE_SHIFT 13\n"),
        ("arch/x86/kernel/setup.c", "#include <asm/page.h>\n"),
        ("arch/arm/mm/init.c", "#include <asm/page.h>\n/* arm */\n"),
        ("crates/geo/Cargo.toml", "[package]\nname = \"geo\"\n"),
        ("crates/geo/src/lib.rs", "mod other;\nmod shape;\n"),
        ("crates/geo/src/shape.rs", SHAPE_RS),
        ("crates/geo/src/other.rs", OTHER_RS),
        ("crates/app/Cargo.toml", "[package]\nname = \"app\"\n"),
        ("crates/app/src/main.rs", "pub struct Point;\n"),
    ] {
        write(d, path, content);
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    write(d, "mm/other.c", &OTHER_C.replace("return 2", "return 20"));
    write(
        d,
        "mm/slab_common.c",
        &format!("{SLAB_COMMON}\nint added(void)\n{{\n\treturn 0;\n}}\n"),
    );
    write(
        d,
        "crates/geo/src/shape.rs",
        &format!("{SHAPE_RS}// edited\n"),
    );
    git(d, &["commit", "-q", "-am", "edit"]);
    dir
}

const TOOLS_SLAB: &str = "#include <linux/slab.h>

void *grab(void)
{
\treturn kmalloc_array(1, 2);
}
";

/// Files of the fixture of a language with a tags query, and how many
/// share their blob with another.
const INDEXED: usize = 17;
const COPIES: usize = 2;

/// A repository of `files` in one commit.
fn repository(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    for (path, content) in files {
        write(dir.path(), path, content);
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "base"]);
    dir
}

struct Setup {
    repo: Repo,
    grammars: Registry,
    spec: DiffSpec,
    changes: Vec<FileChange>,
}

fn setup(dir: &Path, revs: &[&str]) -> Setup {
    let repo = Repo::discover(dir).expect("repo");
    let spec = repo.resolve(revs, false, false).expect("resolves");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    Setup {
        repo,
        grammars: Registry::builtin(),
        spec,
        changes,
    }
}

impl Setup {
    /// A navigator with a store of its own.
    fn navigator(&self, side: view::Side) -> Navigator {
        self.sharing(side, &Shared::new())
    }

    fn sharing(&self, side: view::Side, shared: &Shared) -> Navigator {
        let diff = Diff {
            snapshots: self.spec.snapshots(false).expect("snapshots"),
            changes: &self.changes,
            options: Options::default(),
        };
        Navigator::new(&self.repo, &self.grammars, shared, &diff, side).expect("lists")
    }

    fn origin(&self, path: &str) -> Origin {
        Origin {
            path: path.into(),
            language: (self.grammars.detect(path.as_bytes(), b""))
                .expect("a language")
                .clone(),
        }
    }

    fn lookup(&self, nav: &Navigator, cache: &SymbolCache, name: &str, from: &str) -> Lookup {
        nav.lookup(
            &self.repo,
            cache,
            name,
            &self.origin(from),
            &AtomicBool::new(false),
        )
        .expect("looks up")
    }
}

/// A symbol cache of its own, outside the fixture's working tree.
fn symbols() -> (tempfile::TempDir, SymbolCache) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(dir.path().to_owned(), 1 << 30).symbols();
    (dir, cache)
}

fn built(setup: &Setup, nav: &Navigator, cache: &SymbolCache) {
    nav.build(&setup.repo, cache, 2, &AtomicBool::new(false), |_| {})
        .expect("queries compile");
    assert!(nav.progress().complete());
}

/// Each candidate as `path:first-line rank`, 1-based.
fn found(
    setup: &Setup,
    nav: &Navigator,
    cache: &SymbolCache,
    name: &str,
    from: &str,
) -> Vec<String> {
    (setup.lookup(nav, cache, name, from).candidates.iter())
        .map(|c| format!("{}:{} {:?}", c.path, c.start + 1, c.rank))
        .collect()
}

/// A C name is found in the asking file first, then in what it includes,
/// directly or through another header, then in its folder, then anywhere;
/// copies elsewhere are listed after.
#[test]
fn c_names_rank_by_closeness_to_the_asking_file() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let (_cached, cache) = symbols();
    let nav = setup.navigator(view::Side::New);
    built(&setup, &nav, &cache);
    let from = "mm/slab_common.c";
    assert_eq!(
        found(&setup, &nav, &cache, "kmalloc_array", from),
        [
            "include/linux/slab.h:4 Imported",
            "tools/include/linux/slab.h:4 Elsewhere",
        ]
    );
    assert_eq!(
        found(&setup, &nav, &cache, "list_head", from),
        [
            "include/linux/types.h:1 Imported",
            "tools/include/linux/types.h:1 Elsewhere",
        ]
    );
    assert_eq!(
        found(&setup, &nav, &cache, "helper", from),
        [
            "mm/slab_common.c:4 SameFile",
            "mm/other.c:1 Near",
            "drivers/x.c:1 Elsewhere",
        ]
    );
}

/// An include is found where a compiler of the asking file looks first:
/// `tools/` in `tools/include/`, an architecture's file in its own
/// headers. Outside `arch/`, x86's definitions rank before other
/// architectures'.
#[test]
fn includes_resolve_where_the_asking_file_compiles() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let (_cached, cache) = symbols();
    let nav = setup.navigator(view::Side::New);
    built(&setup, &nav, &cache);
    assert_eq!(
        found(&setup, &nav, &cache, "kmalloc_array", "tools/lib/slab.c"),
        [
            "tools/include/linux/slab.h:4 Imported",
            "include/linux/slab.h:4 Elsewhere",
        ]
    );
    assert_eq!(
        found(&setup, &nav, &cache, "PAGE_SHIFT", "arch/arm/mm/init.c"),
        [
            "arch/arm/include/asm/page.h:1 Imported",
            "arch/x86/include/asm/page.h:1 Elsewhere",
        ]
    );
    assert_eq!(
        found(
            &setup,
            &nav,
            &cache,
            "PAGE_SHIFT",
            "arch/x86/kernel/setup.c"
        ),
        [
            "arch/x86/include/asm/page.h:1 Imported",
            "arch/arm/include/asm/page.h:1 Elsewhere",
        ]
    );
    assert_eq!(
        found(&setup, &nav, &cache, "PAGE_SHIFT", "mm/slab_common.c"),
        [
            "arch/x86/include/asm/page.h:1 Elsewhere",
            "arch/arm/include/asm/page.h:1 Elsewhere",
        ]
    );
}

/// Each candidate says whether its file changed in the range and whether
/// the diff touches its lines.
#[test]
fn candidates_say_whether_the_range_changed_them() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let (_cached, cache) = symbols();
    for side in [view::Side::Old, view::Side::New] {
        let nav = setup.navigator(side);
        built(&setup, &nav, &cache);
        let lookup = setup.lookup(&nav, &cache, "helper", "mm/other.c");
        let facts: Vec<_> = (lookup.candidates.iter())
            .map(|c| (c.path.to_string(), c.changed, c.in_hunk))
            .collect();
        assert_eq!(
            facts,
            [
                ("mm/other.c".to_owned(), true, Some(true)),
                ("mm/slab_common.c".to_owned(), true, Some(false)),
                ("drivers/x.c".to_owned(), false, Some(false)),
            ],
            "{side:?}"
        );
    }
}

/// A Rust method name lists every method of that name with the type each
/// is on, as the receiver's type is unknown; a `use` ranks the file it
/// names above another crate's.
#[test]
fn rust_names_list_every_type_and_follow_uses() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let (_cached, cache) = symbols();
    let nav = setup.navigator(view::Side::New);
    built(&setup, &nav, &cache);
    let lookup = setup.lookup(&nav, &cache, "len", "crates/geo/src/other.rs");
    let methods: Vec<_> = (lookup.candidates.iter())
        .map(|c| (c.container.as_deref(), c.rank))
        .collect();
    assert_eq!(
        methods,
        [
            (Some("Line"), Rank::SameFile),
            (Some("Point"), Rank::Imported)
        ]
    );
    assert_eq!(
        found(&setup, &nav, &cache, "Point", "crates/geo/src/other.rs"),
        [
            "crates/geo/src/shape.rs:1 Imported",
            "crates/app/src/main.rs:1 Elsewhere",
        ]
    );
}

/// Before the index is built, a lookup answers from the asking file, what
/// it includes and the diff's files of its language, read on demand, and
/// says the index is partial.
#[test]
fn the_local_tier_answers_before_the_index() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let (_cached, cache) = symbols();
    let nav = setup.navigator(view::Side::New);
    let lookup = setup.lookup(&nav, &cache, "kmalloc_array", "mm/slab_common.c");
    let paths: Vec<_> = (lookup.candidates.iter())
        .map(|c| c.path.to_string())
        .collect();
    assert_eq!(paths, ["include/linux/slab.h"]);
    // The asking file, its two includes and the diff's other C file; not
    // the Rust file the diff changed.
    assert_eq!(lookup.progress.done, 4);
    assert!(!lookup.progress.complete());
}

/// A cancelled lookup stops and says so.
#[test]
fn a_lookup_stops_when_cancelled() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let (_cached, cache) = symbols();
    let nav = setup.navigator(view::Side::New);
    let cancelled = nav.lookup(
        &setup.repo,
        &cache,
        "kmalloc_array",
        &setup.origin("mm/slab_common.c"),
        &AtomicBool::new(true),
    );
    assert!(matches!(cancelled, Err(Error::Cancelled)), "{cancelled:?}");
    assert_eq!(nav.progress().done, 0);
}

/// A lookup lists the best 200 candidates and says how many there are.
#[test]
fn a_lookup_lists_at_most_200_candidates() {
    let defines = (0..205)
        .map(|n| format!("#define DUP {n}\n"))
        .collect::<Vec<_>>()
        .concat();
    let dir = repository(&[("dup.h", &defines)]);
    let setup = setup(dir.path(), &[]);
    let (_cached, cache) = symbols();
    let nav = setup.navigator(view::Side::New);
    built(&setup, &nav, &cache);
    let lookup = setup.lookup(&nav, &cache, "DUP", "dup.h");
    assert_eq!((lookup.candidates.len(), lookup.total), (200, 205));
}

/// The index is built a chunk at a time: a cancelled chunk gives its files
/// back to be taken again, as does one handed back unrun, and a build
/// stopped part way resumes. Its definitions are stored, so another
/// navigator reads every file back.
#[test]
fn the_index_resumes_and_reads_back_what_it_stored() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let (_cached, cache) = symbols();
    let nav = setup.navigator(view::Side::New);
    let total = nav.progress().total;
    assert_eq!(total, INDEXED);

    let chunk = nav.take_chunk().expect("a chunk");
    let first = chunk.len();
    let done = chunk.run(&setup.repo, &cache, &AtomicBool::new(true));
    assert!(nav.add(done).is_none());
    assert_eq!(nav.progress().done, 0, "cancelled before any file");
    let again = nav.take_chunk().expect("given back");
    assert_eq!(again.len(), first);
    nav.give_back(again);
    let again = nav.take_chunk().expect("handed back");
    assert_eq!(again.len(), first);
    assert!(
        nav.add(again.run(&setup.repo, &cache, &AtomicBool::new(false)))
            .is_none()
    );
    assert_eq!(nav.progress().done, first);

    built(&setup, &nav, &cache);
    // The copies under `tools/` are the same blobs, read once.
    let stats = nav.stats();
    assert_eq!((stats.extracted, stats.cached), (total - COPIES, COPIES));

    let fresh = setup.navigator(view::Side::New);
    built(&setup, &fresh, &cache);
    assert_eq!((fresh.stats().cached, fresh.stats().extracted), (total, 0));
}

/// Navigators sharing a store read each blob's definitions once: the
/// other side of the range reads only the files the range changed, even
/// with the symbol cache gone.
#[test]
fn navigators_sharing_a_store_read_each_blob_once() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let (_cached, cache) = symbols();
    let shared = Shared::new();
    let old = setup.sharing(view::Side::Old, &shared);
    built(&setup, &old, &cache);
    std::fs::remove_dir_all(cache.as_cache().dir()).expect("rm");
    let new = setup.sharing(view::Side::New, &shared);
    built(&setup, &new, &cache);
    assert_eq!(
        (new.stats().extracted, new.stats().cached),
        (3, INDEXED - 3)
    );
}

/// The working tree is its diff's other side with the changes laid over
/// it: an edited file's and an untracked file's definitions are found,
/// read from disk and never cached under a blob id.
#[test]
fn the_working_tree_lays_its_changes_over_the_other_side() {
    let dir = fixture();
    let d = dir.path();
    let edited = format!("{OTHER_C}\nint fresh(void)\n{{\n\treturn 0;\n}}\n");
    write(d, "mm/other.c", &edited);
    write(d, "mm/new.c", "int untracked(void)\n{\n\treturn 0;\n}\n");
    std::fs::remove_file(d.join("drivers/x.c")).expect("rm");
    let setup = setup(d, &[]);
    assert!(matches!(setup.spec, DiffSpec::Uncommitted { .. }));
    let (_cached, cache) = symbols();
    let nav = setup.navigator(view::Side::New);
    built(&setup, &nav, &cache);
    let lookup = |name| {
        setup
            .lookup(&nav, &cache, name, "mm/slab_common.c")
            .candidates
    };
    let fresh = lookup("fresh");
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].path, "mm/other.c");
    assert!(matches!(fresh[0].side, Side::Worktree { .. }));
    assert_eq!(lookup("untracked")[0].path, "mm/new.c");
    let helpers: Vec<_> = lookup("helper")
        .iter()
        .map(|c| c.path.to_string())
        .collect();
    assert_eq!(helpers, ["mm/slab_common.c", "mm/other.c"]);

    let old = setup.navigator(view::Side::Old);
    built(&setup, &old, &cache);
    let none = setup.lookup(&old, &cache, "fresh", "mm/slab_common.c");
    assert_eq!(none.candidates, []);

    let id = git(d, &["hash-object", "mm/other.c"]);
    let id = dffy_core::git::ObjectId::from_hex(id.trim().as_bytes()).expect("an id");
    let c = setup.grammars.named("c").expect("bundled");
    let mut loaded = false;
    highlight::blob_definitions(c, id, edited.len() as u64, &cache, || {
        loaded = true;
        Some(edited.clone().into_bytes())
    })
    .expect("compiles");
    assert!(loaded, "the working tree's file was cached by its id");
}

/// The text search finds the lines using a name no definition has, as
/// matches without a kind, closest files first, and stops at its cap.
#[test]
fn the_text_search_finds_uses_up_to_its_cap() {
    let dir = fixture();
    let setup = setup(dir.path(), &["HEAD~1..HEAD"]);
    let nav = setup.navigator(view::Side::New);
    let from = setup.origin("drivers/x.c");
    let all = nav
        .search_text(&setup.repo, "helper", &from, 10, &AtomicBool::new(false))
        .expect("searches");
    let hits: Vec<_> = (all.matches.iter())
        .map(|c| format!("{}:{} {:?}", c.path, c.start + 1, c.kind))
        .collect();
    assert_eq!(
        hits,
        [
            "drivers/x.c:1 None",
            "mm/other.c:1 None",
            "mm/slab_common.c:4 None",
            "mm/slab_common.c:11 None",
        ]
    );
    assert!(all.complete);
    let capped = nav
        .search_text(&setup.repo, "helper", &from, 2, &AtomicBool::new(false))
        .expect("searches");
    assert_eq!(capped.matches.len(), 2);
    assert!(!capped.complete);
}
