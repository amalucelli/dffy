use std::sync::atomic::AtomicBool;

use crate::cache::Cache;
use crate::diff::Options;
use crate::git::Repo;
use crate::highlight::Registry;
use crate::navigate::{Diff, Navigator, Origin, Shared, Use};
use crate::view;

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

/// Each candidate of `name` used at `at` in `origin`, in a repository of
/// `files`, as `path:line kind`.
fn looked_up(files: &[(&str, &str)], origin: &str, name: &str, at: Option<Use>) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    for (path, text) in files {
        let path = d.join(path);
        std::fs::create_dir_all(path.parent().expect("a folder")).expect("mkdir");
        std::fs::write(path, text).expect("write");
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
    let cache = Cache::new(cache_dir.path().to_owned(), 1 << 22).symbols();
    let go = AtomicBool::new(false);
    nav.build(&repo, &cache, 1, &go, |_| {}).expect("builds");
    let origin = Origin {
        path: origin.into(),
        language: grammars
            .detect(origin.as_bytes(), b"")
            .expect("a language")
            .clone(),
    };
    let found = nav
        .lookup_at(&repo, &cache, name, &origin, at, &go)
        .expect("looks up");
    (found.candidates.iter())
        .map(|c| {
            format!(
                "{}:{} {}",
                c.path,
                c.start + 1,
                c.kind.map_or("text", |k| k.name())
            )
        })
        .collect()
}

/// A name used in a function that a parameter of it defines answers with
/// the parameter first, before what the index has of the name; asked with
/// only its line, the same; without a place, only the index answers.
#[test]
fn a_local_comes_before_the_index() {
    let files = [
        ("a.c", "int size(int count)\n{\n\treturn count + 1;\n}\n"),
        ("b.c", "int count(void)\n{\n\treturn 0;\n}\n"),
    ];
    let at = Use {
        line: 2,
        column: Some(8),
    };
    assert_eq!(
        looked_up(&files, "a.c", "count", Some(at)),
        ["a.c:1 parameter", "b.c:1 function"]
    );
    let line = Use {
        line: 2,
        column: None,
    };
    assert_eq!(
        looked_up(&files, "a.c", "count", Some(line)),
        ["a.c:1 parameter", "b.c:1 function"]
    );
    assert_eq!(looked_up(&files, "a.c", "count", None), ["b.c:1 function"]);
}

/// A member named as a local is no local: only the index answers.
#[test]
fn a_member_is_no_local() {
    let files = [
        (
            "a.go",
            "package p\n\nfunc f(next *Node) *Node {\n\treturn next.next\n}\n",
        ),
        ("b.go", "package p\n\ntype Node struct {\n\tnext *Node\n}\n"),
    ];
    let at = Use {
        line: 3,
        column: Some(13),
    };
    assert_eq!(
        looked_up(&files, "a.go", "next", Some(at)),
        ["b.go:4 field"]
    );
}

/// A name a file imports under another name finds what it names where
/// it's defined, first, though no file defines the name it's used by.
#[test]
fn an_import_alias_resolves_to_its_original() {
    let parse = "export function parse(s: string) {\n  return s;\n}\n";
    let found = looked_up(
        &[
            (
                "app/main.ts",
                "import { parse as p } from \"../z/util\";\np(\"x\");\n",
            ),
            ("z/util.ts", parse),
            ("a/util.ts", "export function p() {}\n"),
        ],
        "app/main.ts",
        "p",
        None,
    );
    assert_eq!(found, ["z/util.ts:1 function", "a/util.ts:1 function"]);
}

/// A barrel's renamed re-export is followed by the name it had.
#[test]
fn a_renamed_reexport_is_followed_by_its_original_name() {
    let found = looked_up(
        &[
            ("app/main.ts", "import { fmt } from \"../lib\";\nfmt(1);\n"),
            (
                "lib/index.ts",
                "export { format as fmt } from \"./z/format\";\n",
            ),
            (
                "lib/z/format.ts",
                "export function format(n: number) {\n  return n;\n}\n",
            ),
        ],
        "app/main.ts",
        "fmt",
        None,
    );
    assert_eq!(
        found,
        ["lib/z/format.ts:1 function", "lib/index.ts:1 reexport"]
    );
}

/// Python's `from a import b as c`.
#[test]
fn a_python_import_alias_resolves_to_its_original() {
    let found = looked_up(
        &[
            ("main.py", "from pkg.util import fmt as f\n\nf(1)\n"),
            ("pkg/__init__.py", ""),
            ("pkg/util.py", "def fmt(x):\n    return x\n"),
            ("a/f.py", "def f():\n    pass\n"),
        ],
        "main.py",
        "f",
        None,
    );
    assert_eq!(
        found,
        [
            "pkg/util.py:1 function",
            "a/f.py:1 function",
            "a/f.py:1 module"
        ]
    );
}

/// Rust's `use … as`, and a `pub use` the imported module re-exports
/// through, by the name it had.
#[test]
fn rust_uses_resolve_through_renames_and_pub_use() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\n"),
        (
            "src/main.rs",
            "use crate::geo::Point as P;\nuse crate::api::Thing;\nfn main() { P::new(); Thing; }\n",
        ),
        ("src/geo.rs", "pub struct Point;\n"),
        ("src/api.rs", "pub use crate::z::Item as Thing;\n"),
        ("src/z.rs", "pub struct Item;\n"),
        ("src/a.rs", "pub struct Thing;\npub struct P;\n"),
    ];
    assert_eq!(
        looked_up(&files, "src/main.rs", "P", None),
        ["src/geo.rs:1 struct", "src/a.rs:2 struct"]
    );
    assert_eq!(
        looked_up(&files, "src/main.rs", "Thing", None),
        [
            "src/z.rs:1 struct",
            "src/a.rs:1 struct",
            "src/api.rs:1 reexport"
        ]
    );
}

/// A Java import names the class file it imports, first, though another
/// class of the name sorts before it.
#[test]
fn a_java_import_resolves_to_the_imported_class() {
    let util = |package: &str| format!("package {package};\n\npublic class Util {{}}\n");
    let found = looked_up(
        &[
            (
                "src/com/acme/app/Main.java",
                "package com.acme.app;\n\nimport com.acme.z.Util;\n\nclass Main { Util u; }\n",
            ),
            ("src/com/acme/z/Util.java", &util("com.acme.z")),
            ("src/com/acme/a/Util.java", &util("com.acme.a")),
        ],
        "src/com/acme/app/Main.java",
        "Util",
        None,
    );
    assert_eq!(
        found,
        [
            "src/com/acme/z/Util.java:3 class",
            "src/com/acme/a/Util.java:3 class"
        ]
    );
}

/// A module a file imports is its file, and one Python imports from
/// outside the repository, as far as can be told, nothing. A Go package,
/// as gopls has it, is its import where it's used, and at its import its
/// clause with the doc comment; one from outside the module is its import.
#[test]
fn an_imported_module_is_its_file_or_the_import() {
    let python = [
        (
            "main.py",
            "import os\nimport pkg.util as u\n\nu.fmt(os.sep)\n",
        ),
        ("pkg/__init__.py", ""),
        ("pkg/util.py", "def fmt(x):\n    return x\n"),
    ];
    assert_eq!(
        looked_up(&python, "main.py", "u", None),
        ["pkg/util.py:1 module"]
    );
    assert_eq!(
        looked_up(&python, "main.py", "os", None),
        Vec::<String>::new()
    );
    let go = [
        ("go.mod", "module example.com/m\n"),
        (
            "main.go",
            "package main\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/m/lib\"\n)\n\nfunc main() { fmt.Println(lib.F()) }\n",
        ),
        ("lib/a.go", "package lib\n\nfunc F() int { return 1 }\n"),
        ("lib/doc.go", "// Package lib does things.\npackage lib\n"),
    ];
    let at = |line| Use { line, column: None };
    assert_eq!(
        looked_up(&go, "main.go", "fmt", Some(at(8))),
        ["main.go:4 import"]
    );
    assert_eq!(
        looked_up(&go, "main.go", "lib", Some(at(8))),
        ["main.go:6 import"]
    );
    assert_eq!(
        looked_up(&go, "main.go", "lib", Some(at(5))),
        ["lib/doc.go:2 module"]
    );
}

/// A default import is what the module exports as its default, by
/// whatever name it has there.
#[test]
fn a_default_import_is_the_default_export() {
    let found = looked_up(
        &[
            (
                "app/main.ts",
                "import Parse from \"../z/parse\";\nParse(\"x\");\n",
            ),
            (
                "z/parse.ts",
                "export default function parse(s: string) {\n  return s;\n}\n",
            ),
        ],
        "app/main.ts",
        "Parse",
        None,
    );
    assert_eq!(found, ["z/parse.ts:1 function"]);
}

/// A member that has an imported name's is the member, not the import,
/// which a use of the name itself is; with no place to tell them apart,
/// the import isn't pinned.
#[test]
fn a_member_is_no_import() {
    let files = [
        (
            "app/main.ts",
            "import { p } from \"lib\";\nconst o = { p: 1 };\no.p;\np();\n",
        ),
        ("a/o.ts", "export interface O {\n  p: number;\n}\n"),
    ];
    let at = Use {
        line: 2,
        column: Some(2),
    };
    assert_eq!(
        looked_up(&files, "app/main.ts", "p", Some(at)),
        ["a/o.ts:2 field"]
    );
    let key = Use {
        line: 1,
        column: Some(12),
    };
    assert_eq!(
        looked_up(&files, "app/main.ts", "p", Some(key)),
        ["a/o.ts:2 field"]
    );
    let call = Use {
        line: 3,
        column: Some(0),
    };
    assert_eq!(
        looked_up(&files, "app/main.ts", "p", Some(call)),
        ["app/main.ts:1 import", "a/o.ts:2 field"]
    );
    assert_eq!(
        looked_up(&files, "app/main.ts", "p", None),
        ["a/o.ts:2 field"]
    );
}

/// Kotlin's `import … as` and C#'s `using X = …` rename what they import.
#[test]
fn kotlin_and_csharp_import_aliases_resolve() {
    let found = looked_up(
        &[
            (
                "src/com/acme/app/Main.kt",
                "package com.acme.app\n\nimport com.acme.z.Util as U\n\nval u = U()\n",
            ),
            (
                "src/com/acme/z/Util.kt",
                "package com.acme.z\n\nclass Util\n",
            ),
        ],
        "src/com/acme/app/Main.kt",
        "U",
        None,
    );
    assert_eq!(found, ["src/com/acme/z/Util.kt:3 class"]);
    let found = looked_up(
        &[
            (
                "App/Main.cs",
                "using U = Acme.Z.Util;\n\nnamespace Acme.App {\n  class Main { U u; }\n}\n",
            ),
            (
                "Z/Util.cs",
                "namespace Acme.Z {\n  public class Util {}\n}\n",
            ),
        ],
        "App/Main.cs",
        "U",
        None,
    );
    assert_eq!(found, ["Z/Util.cs:2 class"]);
}

/// A test's `use` of its own crate by the package's name.
#[test]
fn a_rust_test_uses_its_crate_by_name() {
    let found = looked_up(
        &[
            ("Cargo.toml", "[package]\nname = \"my-app\"\n"),
            ("src/lib.rs", "pub struct Error;\n"),
            ("tests/t.rs", "use my_app::Error as E;\nfn f(_: E) {}\n"),
            ("tests/a.rs", "pub struct E;\n"),
        ],
        "tests/t.rs",
        "E",
        None,
    );
    assert_eq!(found, ["src/lib.rs:1 struct", "tests/a.rs:1 struct"]);
}

/// Go's imports come before its first declaration: one in a string past
/// it is no import.
#[test]
fn a_go_import_in_a_string_is_no_import() {
    let files = [
        ("go.mod", "module example.com/m\n"),
        (
            "main.go",
            "package main\n\nfunc f() string {\n\treturn `\nimport \"fmt\"\n`\n}\n",
        ),
    ];
    assert_eq!(
        looked_up(&files, "main.go", "fmt", None),
        Vec::<String>::new()
    );
}

/// The first candidate of the member `name` used on line `line` (0-based)
/// of `origin`, its last use there.
fn member(files: &[(&str, &str)], origin: &str, name: &str, line: usize) -> String {
    let text = files
        .iter()
        .find(|(p, _)| *p == origin)
        .expect("the origin")
        .1;
    let column = text
        .lines()
        .nth(line)
        .and_then(|l| l.rfind(name))
        .expect("the use");
    let at = Use {
        line,
        column: Some(column),
    };
    let found = looked_up(files, origin, name, Some(at));
    found.into_iter().next().expect("a candidate")
}

/// A member is first looked for in the type its receiver has: one a
/// variable, a parameter or Go's receiver declares, one a constructor or
/// a literal gives it, and `self`'s or `this`'s; over one of another type
/// the asking file defines.
#[test]
fn a_member_is_first_its_receivers_types() {
    let rust = [
        ("src/z.rs", "pub struct Z {\n    pub x: i32,\n}\n"),
        (
            "src/main.rs",
            "struct A {\n    x: i32,\n}\nimpl Z {\n    fn f(&self) -> i32 {\n        self.x\n    }\n}\nfn g(z: &Z) -> i32 {\n    let w = Z::new();\n    z.x + w.x\n}\n",
        ),
    ];
    assert_eq!(member(&rust, "src/main.rs", "x", 5), "src/z.rs:2 field");
    assert_eq!(member(&rust, "src/main.rs", "x", 10), "src/z.rs:2 field");
    let go = [
        ("z.go", "package p\n\ntype Z struct {\n\tname string\n}\n"),
        (
            "main.go",
            "package p\n\ntype A struct {\n\tname string\n}\n\nfunc (s *Z) f() string {\n\treturn s.name\n}\n\nfunc g() string {\n\tc := &Z{}\n\treturn c.name\n}\n",
        ),
    ];
    assert_eq!(member(&go, "main.go", "name", 7), "z.go:4 field");
    assert_eq!(member(&go, "main.go", "name", 12), "z.go:4 field");
    let ts = [
        ("src/z.ts", "export interface Z {\n  retry: number;\n}\n"),
        (
            "src/main.ts",
            "interface A {\n  retry: number;\n}\nclass K {\n  retry = 1;\n  f() {\n    return this.retry;\n  }\n}\nfunction g(o: Z) {\n  return o.retry;\n}\n",
        ),
    ];
    assert_eq!(
        member(&ts, "src/main.ts", "retry", 6),
        "src/main.ts:5 field"
    );
    assert_eq!(member(&ts, "src/main.ts", "retry", 10), "src/z.ts:2 field");
    let python = [
        ("z.py", "class Session:\n    def get(self):\n        pass\n"),
        (
            "main.py",
            "from z import Session\n\n\nclass A:\n    def get(self):\n        pass\n\n\ndef f():\n    s = Session()\n    return s.get()\n",
        ),
    ];
    assert_eq!(member(&python, "main.py", "get", 10), "z.py:2 method");
    let c = [
        ("z.c", "struct z {\n\tint x;\n};\n"),
        (
            "main.c",
            "struct a {\n\tint x;\n};\n\nint f(struct z *p)\n{\n\treturn p->x;\n}\n",
        ),
    ];
    assert_eq!(member(&c, "main.c", "x", 6), "z.c:2 field");
}

/// A member of a module or a namespace imported is first what it defines
/// or re-exports, followed to the module it's imported from, not a class's
/// member of the name.
#[test]
fn a_member_of_an_imported_module_is_first_in_its_files() {
    let package = [
        (
            "pkg/__init__.py",
            "from .errors import Error\nfrom .api import get\nfrom .other import *\n",
        ),
        ("pkg/errors.py", "class Error:\n    get: int\n"),
        ("pkg/other.py", "def get():\n    pass\n"),
        ("pkg/api.py", "def get(url):\n    return url\n"),
        (
            "main.py",
            "import pkg\n\n\ndef get(x):\n    return x\n\n\npkg.get(1)\n",
        ),
    ];
    assert_eq!(
        member(&package, "main.py", "get", 7),
        "pkg/api.py:1 function"
    );
    let python = [
        ("pkg/__init__.py", ""),
        ("pkg/util.py", "def fmt(x):\n    return x\n"),
        (
            "main.py",
            "import pkg.util as u\n\n\ndef fmt(x):\n    return x\n\n\nu.fmt(1)\n",
        ),
    ];
    assert_eq!(
        member(&python, "main.py", "fmt", 7),
        "pkg/util.py:1 function"
    );
    let ts = [
        ("z/util.ts", "export function fmt() {}\n"),
        (
            "app/main.ts",
            "import * as u from \"../z/util\";\nfunction fmt() {}\nu.fmt();\n",
        ),
    ];
    assert_eq!(member(&ts, "app/main.ts", "fmt", 2), "z/util.ts:1 function");
}

/// Rust's `mod x;` is the module's file, `x.rs` or `x/mod.rs`.
#[test]
fn a_rust_module_declaration_is_its_file() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\n"),
        (
            "src/lib.rs",
            "mod geo;\nmod shapes;\n\nfn f() {\n    geo::area();\n}\n",
        ),
        ("src/geo.rs", "pub fn area() {}\n"),
        ("src/shapes/mod.rs", "pub fn draw() {}\n"),
    ];
    let at = |line, column| Use {
        line,
        column: Some(column),
    };
    assert_eq!(
        looked_up(&files, "src/lib.rs", "geo", Some(at(4, 4))),
        ["src/geo.rs:1 module"]
    );
    assert_eq!(
        looked_up(&files, "src/lib.rs", "shapes", Some(at(1, 4))),
        ["src/shapes/mod.rs:1 module"]
    );
}

/// A Go file's `package` clause is its package's, with the doc comment.
#[test]
fn a_go_package_clause_is_the_packages() {
    let files = [
        ("go.mod", "module example.com/m\n"),
        ("lib/a.go", "package lib\n\nfunc F() int { return 1 }\n"),
        ("lib/doc.go", "// Package lib does things.\npackage lib\n"),
    ];
    let at = Use {
        line: 0,
        column: Some(8),
    };
    assert_eq!(
        looked_up(&files, "lib/a.go", "lib", Some(at)),
        ["lib/doc.go:2 module"]
    );
}

/// TypeScript's overload signatures only declare: the implementation is
/// what's found.
#[test]
fn a_typescript_overload_is_its_implementation() {
    let files = [
        (
            "src/f.ts",
            "export function f(a: string): void;\nexport function f(a: number): void;\nexport function f(a: any) {\n  return a;\n}\n",
        ),
        ("src/main.ts", "import { f } from \"./f\";\nf(1);\n"),
    ];
    assert_eq!(
        looked_up(&files, "src/main.ts", "f", None),
        ["src/f.ts:3 function"]
    );
}

/// Rust's `name!` is a macro, before a function of the name; and not one
/// the asking file defines only past the use.
#[test]
fn a_rust_macro_call_is_a_macro_defined_before_it() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\n"),
        (
            "src/lib.rs",
            "mod macros;\n\nfn ensure() {}\n\nfn g() {\n    ensure!(true);\n    later!();\n}\n\nmacro_rules! later {\n    () => {};\n}\n",
        ),
        (
            "src/macros.rs",
            "macro_rules! ensure {\n    ($e:expr) => {};\n}\nmacro_rules! later {\n    () => {};\n}\n",
        ),
    ];
    let at = |line, column| Use {
        line,
        column: Some(column),
    };
    let first = |name, at| looked_up(&files, "src/lib.rs", name, Some(at)).remove(0);
    assert_eq!(first("ensure", at(5, 4)), "src/macros.rs:1 macro");
    assert_eq!(first("later", at(6, 4)), "src/macros.rs:4 macro");
}

/// Among candidates as near: a test's after the others, unless the asking
/// file is a test.
#[test]
fn tests_come_after_the_code_they_test() {
    let files = [
        ("lib/a.test.ts", "export function helper() {}\n"),
        ("lib/z.ts", "export function helper() {}\n"),
        ("app/main.ts", "helper();\n"),
        ("app/main.test.ts", "helper();\n"),
    ];
    assert_eq!(
        looked_up(&files, "app/main.ts", "helper", None),
        ["lib/z.ts:1 function", "lib/a.test.ts:1 function"]
    );
    assert_eq!(
        looked_up(&files, "app/main.test.ts", "helper", None),
        ["lib/a.test.ts:1 function", "lib/z.ts:1 function"]
    );
}

/// Vendored and generated code comes last: by its folder, its name, or
/// the marker generators put at the top.
#[test]
fn vendored_and_generated_code_comes_last() {
    let fmt = "def fmt(x):\n    return x\n";
    let files = [
        ("a/vendor/x/util.py", fmt),
        ("b/api_pb2.py", fmt),
        (
            "c/made.py",
            "# Code generated by a tool. DO NOT EDIT.\ndef fmt(x):\n    return x\n",
        ),
        ("z/util.py", fmt),
        ("app/main.py", "fmt(1)\n"),
    ];
    let mut found = looked_up(&files, "app/main.py", "fmt", None);
    assert_eq!(found.remove(0), "z/util.py:1 function");
    found.sort();
    assert_eq!(
        found,
        [
            "a/vendor/x/util.py:1 function",
            "b/api_pb2.py:1 function",
            "c/made.py:2 function"
        ]
    );
}

/// The asking file's package, by the manifest nearest above it, before
/// another nearer in folders.
#[test]
fn the_asking_files_package_comes_first() {
    let f = "export function f() {}\n";
    let files = [
        ("a/package.json", "{}\n"),
        ("a/b/c/main.ts", "f();\n"),
        ("a/b/other/package.json", "{}\n"),
        ("a/b/other/f.ts", f),
        ("a/x/f.ts", f),
    ];
    assert_eq!(
        looked_up(&files, "a/b/c/main.ts", "f", None),
        ["a/x/f.ts:1 function", "a/b/other/f.ts:1 function"]
    );
}

/// A comparison (`MAX != 0`) is no macro call: a constant the asking
/// file defines past the use stays first.
#[test]
fn a_rust_comparison_is_no_macro_call() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\n"),
        (
            "src/lib.rs",
            "fn f() -> bool {\n    MAX != 0\n}\n\nconst MAX: u32 = 1;\n",
        ),
        ("src/other.rs", "macro_rules! MAX {\n    () => {};\n}\n"),
    ];
    let at = Use {
        line: 1,
        column: Some(4),
    };
    assert_eq!(
        looked_up(&files, "src/lib.rs", "MAX", Some(at)).remove(0),
        "src/lib.rs:5 constant"
    );
}

/// Go's `a, b := x, y` gives each name the value in its place; a loop's
/// binding has no type from its collection.
#[test]
fn a_receiver_is_typed_by_its_own_value() {
    let go = [
        ("z.go", "package p\n\ntype Z struct {\n\tname string\n}\n"),
        (
            "main.go",
            "package p\n\ntype A struct {\n\tname string\n}\n\nfunc g() string {\n\ta, c := A{}, &Z{}\n\treturn a.name + c.name\n}\n",
        ),
    ];
    assert_eq!(member(&go, "main.go", "name", 8), "z.go:4 field");
    let rust = [
        ("src/z.rs", "pub struct Z {\n    pub x: i32,\n}\n"),
        (
            "src/main.rs",
            "struct A {\n    x: i32,\n}\nfn g() {\n    let zs = Z::new();\n    for w in zs {\n        w.x;\n    }\n}\n",
        ),
    ];
    assert_eq!(member(&rust, "src/main.rs", "x", 6), "src/main.rs:2 field");
}

/// A default import of a module whose default export has no name is that
/// export, by what it is.
#[test]
fn an_anonymous_default_export_is_its_line() {
    let found = looked_up(
        &[
            (
                "app/main.ts",
                "import parse from \"../z/parse\";\nparse(\"x\");\n",
            ),
            (
                "z/parse.ts",
                "const n = 1;\nexport default function (s: string) {\n  return s;\n}\n",
            ),
        ],
        "app/main.ts",
        "parse",
        None,
    );
    assert_eq!(found, ["z/parse.ts:2 function"]);
}
