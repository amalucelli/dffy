use std::sync::atomic::AtomicBool;

use super::{Filter, calls};
use crate::cache::Cache;
use crate::diff::Options;
use crate::git::Repo;
use crate::highlight::Registry;
use crate::navigate::{Diff, Fit, Navigator, Origin, Shared};
use crate::view;

/// The lines (1-based) of `path`, holding `text`, that a search for `name`
/// reports with the index built: every use, or only the calls.
fn found(path: &str, text: &str, name: &str, calls_only: bool) -> Vec<usize> {
    let dir = tempfile::tempdir().expect("tempdir");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join(path), text).expect("write");
    git(&["add", "-A"]);
    git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"]);
    let repo = Repo::discover(dir.path()).expect("repo");
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
    let cache = Cache::new(cache_dir.path().to_owned(), 1 << 22).symbols();
    let go = AtomicBool::new(false);
    nav.build(&repo, &cache, 1, &go, |_| {}).expect("builds");
    let origin = Origin {
        path: path.into(),
        language: grammars
            .detect(path.as_bytes(), b"")
            .expect("a language")
            .clone(),
    };
    let filter = Filter {
        calls_only,
        in_diff: false,
    };
    let mut search = (nav.references(&repo, name, &origin, filter, &go)).expect("searches");
    let mut lines = Vec::new();
    while !search.done() {
        lines.extend(
            search
                .step(&nav, &repo, || false)
                .iter()
                .map(|r| r.candidate.start + 1),
        );
    }
    lines
}

/// Each use on a line is tried as a call, not only its first.
#[test]
fn a_call_counts_whatever_else_its_line_uses() {
    let rust = "struct A {\n    len: usize,\n}\nimpl A {\n    fn set(&mut self) {\n        self.len = self.len();\n        let len = len(1);\n        let n = len;\n    }\n}\n";
    assert_eq!(found("a.rs", rust, "len", true), [6, 7]);
    let js = "function f(size) {\n  const size2 = size;\n  let size = size(1);\n}\n";
    assert_eq!(found("a.js", js, "size", true), [3]);
    let string = "def f():\n    return g(\"g(1)\")\n\n\ndef h():\n    return \"g(1)\"\n";
    assert_eq!(found("a.py", string, "g", true), [2], "not one in a string");
}

/// A recursive call is a call: the definition's own line is what's left
/// out, with every call in its body kept.
#[test]
fn a_recursive_call_counts_in_its_own_definition() {
    let rust = "fn fact(n: u64) -> u64 {\n    n * fact(n - 1)\n}\n";
    assert_eq!(found("a.rs", rust, "fact", true), [2]);
    assert_eq!(found("a.rs", rust, "fact", false), [2]);
    let js = "const fact = (n) => {\n  return n * fact(n - 1);\n};\n";
    assert_eq!(found("a.js", js, "fact", true), [2]);
    let go = "package p\n\nfunc fact(n int) int {\n\treturn n * fact(n-1)\n}\n";
    assert_eq!(found("a.go", go, "fact", true), [4]);
    let python = "def fact(n):\n    return n * fact(n - 1)\n\n\nclass Foo:\n    @classmethod\n    def make(cls):\n        return Foo(1)\n";
    assert_eq!(found("a.py", python, "fact", true), [2]);
    assert_eq!(found("a.py", python, "Foo", true), [8]);
}

/// On a definition's line only the use that defines it is left out: not one
/// of a type of its name before it, nor a call after it.
#[test]
fn a_definitions_line_counts_when_it_also_uses_the_name() {
    let one = "fn fact(n: u64) -> u64 { n * fact(n - 1) }\nfn g() -> u64 { fact(2) }\n";
    assert_eq!(found("a.rs", one, "fact", true), [1, 2]);
    assert_eq!(
        found("a.js", "const f = (n) => f(n - 1);\n", "f", true),
        [1]
    );
    let c = "struct foo { int x; };\nstruct foo *foo(int n)\n{\n\treturn n ? foo(n - 1) : 0;\n}\n";
    assert_eq!(found("a.c", c, "foo", true), [4], "its own name is no call");
    let split = "struct foo { int x; };\nstruct foo *\nfoo(int n)\n{\n\treturn foo(n - 1);\n}\n";
    assert_eq!(found("a.c", split, "foo", true), [5]);
}

/// What follows a name in a call: arguments, after type arguments. A
/// comparison's `<` isn't one.
#[test]
fn type_arguments_are_told_from_comparisons() {
    let is = |line: &str, name: &str| {
        let at = line.find(name).expect("the name");
        calls(&line.as_bytes()[..at], &line.as_bytes()[at + name.len()..])
    };
    assert!(!is("if count < max && len > (2) {", "count"));
    assert!(!is("if count<max && len>(2) {", "count"));
    assert!(!is("ok = count < (max)", "count"));
    assert!(is(
        "let v = it.collect::<Vec<Box<dyn Fn() -> u32>>>();",
        "collect"
    ));
    assert!(is("it.take::<dyn Iterator<Item = u32>>(1)", "take"));
    assert!(is("make<std::vector<int>>(1);", "make"));
    assert!(is("use<(a: A) => B>(x);", "use"));
    assert!(is("Map[int](xs)", "Map"));
    assert!(is("println!(\"x\")", "println"));
    assert!(!is("if name != (x) {", "name"));
    assert!(!is("fun name(x: Int)", "name"));
}

/// A header that reads as C++ is searched as C++: a raw string's lines are
/// no uses.
#[test]
fn a_cpp_header_is_searched_as_cpp() {
    let header =
        "namespace a {\nconst char *s = R\"(\nfrob();\n)\";\nint g() { return frob(); }\n}\n";
    assert_eq!(found("a.h", header, "frob", false), [5]);
}

/// A search in the diff searches what the diff has now: a file it took in
/// since the search started is searched, one it lost isn't.
#[test]
fn a_search_in_the_diff_follows_its_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    let write = |name: &str, n: usize| {
        let text = format!("int {name}(void)\n{{\n\treturn frob({n});\n}}\n");
        std::fs::write(d.join(format!("{name}.c")), text).expect("write");
    };
    for name in ["a", "b", "c"] {
        write(name, 1);
    }
    git(&["add", "-A"]);
    git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"]);
    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], false, false).expect("resolves");
    let snapshots = spec.snapshots(false).expect("snapshots");
    let changes = || {
        let mut out = Vec::new();
        let repo = Repo::discover(d).expect("repo");
        repo.for_each_change(&spec, &[], |change| {
            out.push(change);
            std::ops::ControlFlow::Continue(())
        })
        .expect("lists");
        out
    };
    write("a", 10);
    write("c", 100);
    let before = changes();
    let grammars = Registry::builtin();
    let diff = |changes| Diff {
        snapshots,
        changes,
        options: Options::default(),
    };
    let nav = Navigator::new(
        &repo,
        &grammars,
        &Shared::new(),
        &diff(&before),
        view::Side::Old,
    )
    .expect("lists");
    let origin = Origin {
        path: "a.c".into(),
        language: grammars.named("c").expect("bundled").clone(),
    };
    let filter = Filter {
        calls_only: false,
        in_diff: true,
    };
    let go = AtomicBool::new(false);
    let mut search = (nav.references(&repo, "frob", &origin, filter, &go)).expect("searches");
    let paths = |found: Vec<super::Reference>| -> Vec<String> {
        found.iter().map(|r| r.candidate.path.to_string()).collect()
    };
    // One file a step.
    let one = || {
        let mut asked = 0;
        move || {
            asked += 1;
            asked > 1
        }
    };
    assert_eq!(paths(search.step(&nav, &repo, one())), ["a.c"]);
    write("b", 10);
    write("c", 1);
    let after = changes();
    assert_eq!(nav.refit(&repo, &diff(&after)), Fit::Changes);
    assert_eq!(paths(search.step(&nav, &repo, || false)), ["b.c"]);
    assert!(search.done());
}
