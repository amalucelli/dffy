use std::sync::atomic::AtomicBool;

use crate::cache::Cache;
use crate::diff::Options;
use crate::git::Repo;
use crate::highlight::Registry;
use crate::navigate::{Candidate, Diff, Navigator, Origin, Rank, Shared};
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

/// The definitions of `name` seen from `origin` in a repository of
/// `files`, committed and fully indexed: each one's path and rank, best
/// first.
fn ranked(files: &[(&str, &str)], origin: &str, name: &str) -> Vec<(String, Rank)> {
    (candidates(files, origin, name).into_iter())
        .map(|c| (c.path.to_string(), c.rank))
        .collect()
}

/// [`ranked`]'s candidates, each as `path:line kind in container`.
fn listed(files: &[(&str, &str)], origin: &str, name: &str) -> Vec<String> {
    (candidates(files, origin, name).iter())
        .map(|c| {
            let kind = c.kind.map_or("text", |k| k.name());
            let container = (c.container.as_deref()).map_or(String::new(), |k| format!(" in {k}"));
            format!("{}:{} {kind}{container}", c.path, c.start + 1)
        })
        .collect()
}

fn candidates(files: &[(&str, &str)], origin: &str, name: &str) -> Vec<Candidate> {
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
    let language = grammars
        .detect(origin.as_bytes(), b"")
        .expect("a language")
        .clone();
    let origin = Origin {
        path: origin.into(),
        language,
    };
    nav.lookup(&repo, &cache, name, &origin, &go)
        .expect("looks up")
        .candidates
}

fn owned(want: &[(&str, Rank)]) -> Vec<(String, Rank)> {
    want.iter().map(|(p, r)| ((*p).to_owned(), *r)).collect()
}

/// A TSX file importing a TypeScript file by a relative path without its
/// extension: the imported file's definition comes first, though another
/// sorts before it by path.
#[test]
fn a_typescript_import_ranks_the_imported_file_first() {
    let format = "export function format(n: number) {\n  return n;\n}\n";
    let found = ranked(
        &[
            (
                "app/main.tsx",
                "import { format } from \"../z/util\";\nexport const App = () => <p>{format(1)}</p>;\n",
            ),
            ("z/util.ts", format),
            ("a/util.ts", format),
        ],
        "app/main.tsx",
        "format",
    );
    assert_eq!(
        found,
        owned(&[
            ("z/util.ts", Rank::Imported),
            ("a/util.ts", Rank::Elsewhere)
        ])
    );
}

/// A package import from a file in a folder below the package's root,
/// and a relative import of a sibling module.
#[test]
fn python_imports_find_packages_above_and_relative_modules() {
    let fmt = "def fmt(x):\n    return x\n";
    let found = ranked(
        &[
            (
                "pkg/app/main.py",
                "from pkg.util import fmt\nfrom . import peer\n\nfmt(peer.run())\n",
            ),
            ("pkg/app/peer.py", "def run():\n    pass\n"),
            ("pkg/util.py", fmt),
            ("a/util.py", fmt),
        ],
        "pkg/app/main.py",
        "fmt",
    );
    assert_eq!(
        found,
        owned(&[
            ("pkg/util.py", Rank::Imported),
            ("a/util.py", Rank::Elsewhere)
        ])
    );
}

/// An import of the module's own package, by the module path in
/// `go.mod`, is every file of the package's folder but its tests.
#[test]
fn a_go_import_of_the_modules_package_is_its_folder() {
    let parse = "package util\n\nfunc Parse() {}\n";
    let found = ranked(
        &[
            ("go.mod", "module example.com/app\n\ngo 1.22\n"),
            (
                "cmd/app/main.go",
                "package main\n\nimport (\n\t\"fmt\"\n\tu \"example.com/app/z/util\"\n)\n\nfunc main() { u.Parse() }\n",
            ),
            ("z/util/parse.go", parse),
            ("z/util/parse_test.go", parse),
            ("a/util/parse.go", parse),
        ],
        "cmd/app/main.go",
        "Parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("z/util/parse.go", Rank::Imported),
            ("a/util/parse.go", Rank::Elsewhere),
            ("z/util/parse_test.go", Rank::Elsewhere),
        ])
    );
}

/// An imported class's file is found under the folder the importing
/// file's own package starts in.
#[test]
fn a_java_import_is_the_class_file_under_the_package_root() {
    let util = |package: &str| {
        format!(
            "package {package};\n\npublic class Util {{\n    static int parse() {{ return 0; }}\n}}\n"
        )
    };
    let found = ranked(
        &[
            (
                "src/main/java/com/acme/app/Main.java",
                "package com.acme.app;\n\nimport com.acme.z.Util;\n\nclass Main { int x = Util.parse(); }\n",
            ),
            ("src/main/java/com/acme/z/Util.java", &util("com.acme.z")),
            ("src/main/java/com/acme/a/Util.java", &util("com.acme.a")),
        ],
        "src/main/java/com/acme/app/Main.java",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("src/main/java/com/acme/z/Util.java", Rank::Imported),
            ("src/main/java/com/acme/a/Util.java", Rank::Elsewhere),
        ])
    );
}

/// A C++ file's `#include` of a header, which is C's (`.h`), finds the
/// header's definitions first.
#[test]
fn a_cpp_include_of_a_c_header_ranks_it_first() {
    let header = "static inline int parse(void)\n{\n\treturn 0;\n}\n";
    let found = ranked(
        &[
            (
                "app/main.cpp",
                "#include \"../z/util.h\"\n\nint main() { return parse(); }\n",
            ),
            ("z/util.h", header),
            ("a/util.h", header),
        ],
        "app/main.cpp",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[("z/util.h", Rank::Imported), ("a/util.h", Rank::Elsewhere)])
    );
}

/// A C# `using` is every file of the namespace's folder, under the root
/// the file's own namespace gives, whose first part no folder spells.
#[test]
fn a_csharp_using_is_the_namespaces_folder() {
    let util = |ns: &str| {
        format!(
            "namespace Acme.{ns};\n\nstatic class Util\n{{\n    public static int Parse() => 0;\n}}\n"
        )
    };
    let found = ranked(
        &[
            (
                "src/App/Main.cs",
                "using System;\nusing Acme.Z;\n\nnamespace Acme.App;\n\nclass Main { int x = Util.Parse(); }\n",
            ),
            ("src/Z/Util.cs", &util("Z")),
            ("src/A/Util.cs", &util("A")),
        ],
        "src/App/Main.cs",
        "Parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("src/Z/Util.cs", Rank::Imported),
            ("src/A/Util.cs", Rank::Elsewhere)
        ])
    );
}

/// A Kotlin import of a top-level function, which no file is named after,
/// is the files of its package's folder.
#[test]
fn a_kotlin_function_import_is_its_packages_folder() {
    let util = |package: &str| format!("package {package}\n\nfun parse(): Int = 0\n");
    let found = ranked(
        &[
            (
                "src/main/kotlin/com/acme/app/Main.kt",
                "package com.acme.app\n\nimport com.acme.z.parse\n\nval x = parse()\n",
            ),
            ("src/main/kotlin/com/acme/z/Strings.kt", &util("com.acme.z")),
            ("src/main/kotlin/com/acme/a/Strings.kt", &util("com.acme.a")),
        ],
        "src/main/kotlin/com/acme/app/Main.kt",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("src/main/kotlin/com/acme/z/Strings.kt", Rank::Imported),
            ("src/main/kotlin/com/acme/a/Strings.kt", Rank::Elsewhere),
        ])
    );
}

/// `require` of a folder is its index file.
#[test]
fn a_javascript_require_of_a_folder_is_its_index() {
    let make = "function make() {}\nmodule.exports.make = make;\n";
    let found = ranked(
        &[
            (
                "src/app.js",
                "const { make } = require(\"./z\");\nmake();\n",
            ),
            ("src/z/index.js", make),
            ("src/a/index.js", make),
        ],
        "src/app.js",
        "make",
    );
    assert_eq!(
        found,
        owned(&[
            ("src/z/index.js", Rank::Imported),
            ("src/a/index.js", Rank::Elsewhere)
        ])
    );
}

/// A name imported through a barrel file's `export { … } from` finds its
/// definition in the file the barrel re-exports from, first, and the
/// re-export after it.
#[test]
fn a_typescript_barrel_is_followed_to_the_definition() {
    let parse = "export function parse(s: string) {\n  return s;\n}\n";
    let found = listed(
        &[
            (
                "app/main.ts",
                "import { parse } from \"../lib\";\nparse(\"x\");\n",
            ),
            ("lib/index.ts", "export { parse } from \"./z/parse\";\n"),
            ("lib/z/parse.ts", parse),
            ("a/parse.ts", parse),
        ],
        "app/main.ts",
        "parse",
    );
    assert_eq!(
        found,
        [
            "lib/z/parse.ts:1 function",
            "a/parse.ts:1 function",
            "lib/index.ts:1 reexport"
        ]
    );
}

/// A package's `__init__.py` re-exports what it imports, and only there
/// does a relative import count as a re-export.
#[test]
fn a_python_packages_init_is_followed_to_the_definition() {
    let fmt = "def fmt(x):\n    return x\n";
    let found = listed(
        &[
            ("main.py", "from pkg import fmt\n\nfmt(1)\n"),
            ("pkg/__init__.py", "from .z import fmt\n"),
            ("pkg/z.py", fmt),
            ("pkg/user.py", "from .z import fmt\n"),
            ("a/z.py", fmt),
        ],
        "main.py",
        "fmt",
    );
    assert_eq!(
        found,
        [
            "pkg/z.py:1 function",
            "a/z.py:1 function",
            "pkg/__init__.py:1 reexport"
        ]
    );
}

/// A Rust module's `pub use` is followed from the module a file uses.
#[test]
fn a_rust_pub_use_is_followed_to_the_definition() {
    let parse = "pub fn parse() {}\n";
    let found = listed(
        &[
            ("Cargo.toml", "[package]\nname = \"app\"\n"),
            (
                "src/main.rs",
                "use crate::api::parse;\nfn main() { parse() }\n",
            ),
            ("src/api.rs", "pub use crate::z::parse;\n"),
            ("src/z.rs", parse),
            ("src/a.rs", parse),
        ],
        "src/main.rs",
        "parse",
    );
    assert_eq!(
        found,
        [
            "src/z.rs:1 function",
            "src/a.rs:1 function",
            "src/api.rs:1 reexport"
        ]
    );
}

/// Of two methods of a name in files equally far, the one on a type the
/// asking file names comes first.
#[test]
fn a_member_of_a_type_the_file_names_ranks_first() {
    let found = listed(
        &[
            (
                "app/main.go",
                "package main\n\nfunc run(z *Zone) { z.Close() }\n",
            ),
            (
                "a/a.go",
                "package a\n\ntype Area struct{}\n\nfunc (x *Area) Close() {}\n",
            ),
            (
                "b/b.go",
                "package b\n\ntype Zone struct {\n\tClose func()\n}\n",
            ),
        ],
        "app/main.go",
        "Close",
    );
    assert_eq!(found, ["b/b.go:4 field in Zone", "a/a.go:5 method in Area"]);
}

/// Ruby's `require_relative` names a file next to the requiring one.
#[test]
fn a_ruby_require_relative_ranks_the_file_first() {
    let parse = "def parse(s)\n  s\nend\n";
    let found = ranked(
        &[
            ("app/main.rb", "require_relative \"../z/util\"\nparse(1)\n"),
            ("z/util.rb", parse),
            ("a/util.rb", parse),
        ],
        "app/main.rb",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("z/util.rb", Rank::Imported),
            ("a/util.rb", Rank::Elsewhere)
        ])
    );
}

/// A PHP `use` of a class is its file under the folder the namespace
/// starts in, by the file's own namespace.
#[test]
fn a_php_use_is_the_class_file_under_the_namespace_root() {
    let util = |ns: &str| {
        format!(
            "<?php\nnamespace App\\{ns};\n\nclass Util {{\n    public static function parse() {{}}\n}}\n"
        )
    };
    let found = ranked(
        &[
            (
                "src/Http/Main.php",
                "<?php\nnamespace App\\Http;\n\nuse App\\Z\\Util;\n\nUtil::parse();\n",
            ),
            ("src/Z/Util.php", &util("Z")),
            ("src/A/Util.php", &util("A")),
        ],
        "src/Http/Main.php",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("src/Z/Util.php", Rank::Imported),
            ("src/A/Util.php", Rank::Elsewhere)
        ])
    );
}

/// A Scala import of a group of names from a package.
#[test]
fn a_scala_grouped_import_is_each_names_file() {
    let util =
        |package: &str| format!("package {package}\n\nobject Util:\n  def parse(s: String) = s\n");
    let found = ranked(
        &[
            (
                "src/main/scala/acme/app/Main.scala",
                "package acme.app\n\nimport acme.z.{Util, Other}\n\nval x = Util.parse(\"a\")\n",
            ),
            ("src/main/scala/acme/z/Util.scala", &util("acme.z")),
            ("src/main/scala/acme/a/Util.scala", &util("acme.a")),
        ],
        "src/main/scala/acme/app/Main.scala",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("src/main/scala/acme/z/Util.scala", Rank::Imported),
            ("src/main/scala/acme/a/Util.scala", Rank::Elsewhere),
        ])
    );
}

/// Zig's `@import` of a file next to the importing one.
#[test]
fn a_zig_import_of_a_file_ranks_it_first() {
    let parse = "pub fn parse() void {}\n";
    let found = ranked(
        &[
            (
                "src/main.zig",
                "const util = @import(\"../z/util.zig\");\ntest { util.parse(); }\n",
            ),
            ("z/util.zig", parse),
            ("a/util.zig", parse),
        ],
        "src/main.zig",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("z/util.zig", Rank::Imported),
            ("a/util.zig", Rank::Elsewhere)
        ])
    );
}

/// Lua's `require` of a dotted module, found from a folder above.
#[test]
fn a_lua_require_is_the_modules_file() {
    let parse = "local M = {}\nfunction M.parse(s) return s end\nreturn M\n";
    let found = ranked(
        &[
            (
                "lua/app/main.lua",
                "local util = require(\"z.util\")\nutil.parse(1)\n",
            ),
            ("lua/z/util.lua", parse),
            ("lua/a/util.lua", parse),
        ],
        "lua/app/main.lua",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("lua/z/util.lua", Rank::Imported),
            ("lua/a/util.lua", Rank::Elsewhere)
        ])
    );
}

/// An Elixir alias names a module's file under `lib/`, its segments in
/// snake case.
#[test]
fn an_elixir_alias_is_the_modules_file() {
    let util = |name: &str| format!("defmodule MyApp.{name}.Util do\n  def parse(s), do: s\nend\n");
    let found = ranked(
        &[
            (
                "lib/my_app/web/main.ex",
                "defmodule MyApp.Web.Main do\n  alias MyApp.HTTPZone.Util\n  def run, do: Util.parse(1)\nend\n",
            ),
            ("lib/my_app/http_zone/util.ex", &util("HTTPZone")),
            ("lib/my_app/area/util.ex", &util("Area")),
        ],
        "lib/my_app/web/main.ex",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[
            ("lib/my_app/http_zone/util.ex", Rank::Imported),
            ("lib/my_app/area/util.ex", Rank::Elsewhere),
        ])
    );
}

/// A script's `source` of a path under its own folder's variable.
#[test]
fn a_bash_source_ranks_the_sourced_script_first() {
    let parse = "parse() {\n  echo \"$1\"\n}\n";
    let found = ranked(
        &[
            (
                "bin/run.sh",
                "source \"$(dirname \"$0\")/../z/lib.sh\"\nparse x\n",
            ),
            ("z/lib.sh", parse),
            ("a/lib.sh", parse),
        ],
        "bin/run.sh",
        "parse",
    );
    assert_eq!(
        found,
        owned(&[("z/lib.sh", Rank::Imported), ("a/lib.sh", Rank::Elsewhere)])
    );
}

/// A Python module's name finds its file, a package's its `__init__.py`.
#[test]
fn a_python_module_name_is_its_file() {
    let found = listed(
        &[
            (
                "main.py",
                "import pkg.util\nfrom pkg import parse\n\npkg.util.run()\n",
            ),
            ("pkg/__init__.py", ""),
            ("pkg/util.py", "def run():\n    pass\n"),
            ("other/utility.py", "def util():\n    pass\n"),
        ],
        "main.py",
        "util",
    );
    assert_eq!(
        found,
        ["pkg/util.py:1 module", "other/utility.py:1 function"]
    );
    let package = listed(
        &[("main.py", "import pkg\n"), ("pkg/__init__.py", "")],
        "main.py",
        "pkg",
    );
    assert_eq!(package, ["pkg/__init__.py:1 module"]);
}

/// Swift imports modules, not files, so a name defined in another file of
/// the asking file's folder ranks as near, and its type's method comes
/// before one on a type the file doesn't name.
#[test]
fn a_swift_name_in_another_file_of_the_folder_is_near() {
    let found = listed(
        &[
            ("Sources/App/Main.swift", "let p = Point.make()\n"),
            (
                "Sources/App/Point.swift",
                "struct Point {\n    static func make() -> Point { Point() }\n}\n",
            ),
            (
                "Sources/Other/Area.swift",
                "struct Area {\n    static func make() -> Area { Area() }\n}\n",
            ),
        ],
        "Sources/App/Main.swift",
        "make",
    );
    assert_eq!(
        found,
        [
            "Sources/App/Point.swift:2 method in Point",
            "Sources/Other/Area.swift:2 method in Area"
        ]
    );
    let ranks = ranked(
        &[
            ("Sources/App/Main.swift", "let p = Point.make()\n"),
            (
                "Sources/App/Point.swift",
                "struct Point {\n    static func make() -> Point { Point() }\n}\n",
            ),
        ],
        "Sources/App/Main.swift",
        "make",
    );
    assert_eq!(ranks, owned(&[("Sources/App/Point.swift", Rank::Near)]));
}

/// A header, read as C, sees the definitions of the C++ files that
/// implement it, as a C++ file sees the header's.
#[test]
fn a_c_header_sees_cpp_definitions() {
    let found = listed(
        &[
            ("include/util.h", "int parse(const char *s);\n"),
            (
                "src/util.cpp",
                "int parse(const char *s)\n{\n    return 0;\n}\n",
            ),
        ],
        "include/util.h",
        "parse",
    );
    assert_eq!(found, ["src/util.cpp:1 function"]);
}

/// A barrel's `export { … } from` written over several lines is followed
/// as one statement.
#[test]
fn a_barrel_statement_over_lines_is_followed() {
    let parse = "export function parse(s: string) {\n  return s;\n}\n";
    let found = listed(
        &[
            (
                "app/main.ts",
                "import { parse } from \"../lib\";\nparse(\"x\");\n",
            ),
            (
                "lib/index.ts",
                "export {\n  parse,\n  other,\n} from \"./z/parse\";\n",
            ),
            ("lib/z/parse.ts", parse),
            ("a/parse.ts", parse),
        ],
        "app/main.ts",
        "parse",
    );
    assert_eq!(
        found,
        [
            "lib/z/parse.ts:1 function",
            "a/parse.ts:1 function",
            "lib/index.ts:1 reexport"
        ]
    );
}

/// What's read of a statement that never ends, and of a file of nothing but
/// imports, is bounded.
#[test]
fn statements_and_imports_are_read_up_to_a_bound() {
    use super::{MAX_IMPORTS, STATEMENT_BYTES, aliased_uses, reexported_from, statement};
    assert_eq!(statement(b" a from \"b\"; c").len(), 11);
    assert_eq!(statement(b" a\nimport b").len(), 2);
    assert_eq!(
        statement("a ".repeat(STATEMENT_BYTES).as_bytes()).len(),
        STATEMENT_BYTES
    );
    let exports = "export { a } from \"./b\";\n".repeat(10_000);
    assert_eq!(reexported_from(exports.as_bytes()).len(), MAX_IMPORTS);
    let uses = "use a::b;\n".repeat(10_000);
    assert_eq!(aliased_uses(uses.as_bytes()).len(), MAX_IMPORTS);
}
