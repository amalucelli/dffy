use std::fmt::Write as _;
use std::sync::LazyLock;

use crate::diff::Text;
use crate::highlight::{CACHE_MIN_BYTES, FileSyntax, Highlighter, Registry, Source, parse};

use super::*;

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

fn scopes_of(lang: &str, source: &str) -> Scopes {
    let lang = GRAMMARS.named(lang).expect("a bundled language");
    let text = Text::new(source.into());
    parse(lang, text.bytes())
        .expect("parses")
        .scopes(&text)
        .expect("a scope query")
}

/// Each scope as `first-last label`, 1-based, indented by depth.
fn outline(lang: &str, source: &str) -> String {
    let scopes = scopes_of(lang, source);
    let mut out = String::new();
    for (i, s) in scopes.scopes.iter().enumerate() {
        let mut depth = 0;
        let mut p = s.parent;
        while p != NONE {
            depth += 1;
            p = scopes.scopes[p as usize].parent;
        }
        let _ = writeln!(
            out,
            "{}{}-{} {}",
            "  ".repeat(depth),
            s.start + 1,
            s.end + 1,
            scopes.label(i)
        );
    }
    out
}

/// The scopes enclosing 1-based `line`, joined as the title shows them.
fn at(lang: &str, source: &str, line: usize) -> String {
    scopes_of(lang, source).at(line - 1).join(" › ")
}

#[test]
fn every_bundled_scope_query_compiles() {
    let grammars = Registry::builtin();
    let with: Vec<_> = grammars
        .languages()
        .iter()
        .filter(|l| l.has_scopes())
        .map(|l| {
            let query = l.scope_query().expect("compiles");
            assert!(
                query.roots.is_some(),
                "{}: a pattern's first node isn't named",
                l.name()
            );
            l.name()
        })
        .collect();
    assert_eq!(
        with,
        [
            "c",
            "cpp",
            "go",
            "java",
            "javascript",
            "kotlin",
            "python",
            "rust",
            "swift",
            "tsx",
            "typescript"
        ]
    );
}

const RUST: &str = "struct Error {
    code: u32,
}

impl Error {
    pub(crate) async fn fatal(&self) -> bool {
        let f = |x: u32| {
            x + 1
        };
        self.code > 1
    }
}

impl<T> std::fmt::Display for Wrap<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

mod inner {
    trait T {
        fn a(&self) {}
    }
    enum E { A }
}
mod outside;
";

/// Closures aren't scopes: they have no name to show, and the function
/// around them says where the code is.
#[test]
fn rust_scopes_nest_and_skip_closures() {
    assert_eq!(
        outline("rust", RUST),
        "1-3 struct Error
5-12 impl Error
  6-11 fn fatal
14-18 impl std::fmt::Display for Wrap<T>
  15-17 fn fmt
20-25 mod inner
  21-23 trait T
    22-22 fn a
  24-24 enum E
"
    );
    assert_eq!(at("rust", RUST, 8), "impl Error › fn fatal");
    assert_eq!(at("rust", RUST, 6), "impl Error › fn fatal");
    assert_eq!(at("rust", RUST, 12), "impl Error");
    assert_eq!(at("rust", RUST, 4), "");
    assert_eq!(at("rust", RUST, 22), "mod inner › trait T › fn a");
    // After a sibling that ended, the scope around both.
    assert_eq!(at("rust", RUST, 24), "mod inner › enum E");
    assert_eq!(at("rust", RUST, 26), "");
}

#[test]
fn go_methods_keep_their_receiver() {
    let source = "package main

type Server struct {
\taddr string
}

func (s *Server) Handle(w http.ResponseWriter) {
\tgo func() {
\t\tx()
\t}()
}

func main() {
}
";
    assert_eq!(
        outline("go", source),
        "3-5 type Server
7-11 func (s *Server) Handle
13-14 func main
"
    );
    assert_eq!(at("go", source, 9), "func (s *Server) Handle");
}

#[test]
fn python_classes_and_nested_defs() {
    let source = "class A:
    def f(self):
        def g():
            pass
        return g

    @staticmethod
    async def h():
        pass

def top():
    return lambda: 1
";
    assert_eq!(
        outline("python", source),
        "1-9 class A
  2-5 def f
    3-4 def g
  8-9 def h
11-12 def top
"
    );
    // A blank line between methods is in the class.
    assert_eq!(at("python", source, 6), "class A");
    assert_eq!(at("python", source, 10), "");
}

const TS: &str = "export class Store<T> {
  items: T[] = [];
  add(item: T) {
    this.items.forEach((i) => {
      log(i);
    });
  }
}

interface Shape {
  area(): number;
}

function make() {
  return 1;
}

const handler = async (req) => {
  return req;
};

namespace NS {
  enum Color { Red }
}
";

/// A method or an arrow function bound to a name has no keyword: its name
/// shows with `()`. Anonymous callbacks aren't scopes.
#[test]
fn typescript_classes_methods_and_named_arrows() {
    let expected = "1-8 class Store
  3-7 add()
10-12 interface Shape
14-16 function make
18-20 handler()
22-24 namespace NS
  23-23 enum Color
";
    assert_eq!(outline("typescript", TS), expected);
    assert_eq!(outline("tsx", TS), expected);
    assert_eq!(at("typescript", TS, 5), "class Store › add()");
}

#[test]
fn javascript_shares_the_typescript_shapes() {
    let source = "class A {
  b() {
    return 1;
  }
}
function* gen() {
  yield 1;
}
const c = function () {
  return 2;
};
";
    assert_eq!(
        outline("javascript", source),
        "1-5 class A
  2-4 b()
6-8 function gen
9-11 c()
"
    );
}

#[test]
fn c_functions_and_types() {
    let source = "struct point {
\tint x;
};

static int *make(int n)
{
\treturn 0;
}

int main(void)
{
\treturn 0;
}
";
    assert_eq!(
        outline("c", source),
        "1-3 struct point
5-8 make()
10-13 main()
"
    );
    let cpp = "namespace ns {
class Box {
  int size() { return 0; }
};
}
";
    assert_eq!(
        outline("cpp", cpp),
        "1-5 namespace ns
  2-4 class Box
    3-3 size()
"
    );
}

#[test]
fn java_classes_methods_and_lambdas() {
    let source = "public class App {
    public App() {}
    void run() {
        Runnable r = () -> {
            go();
        };
    }
    interface Cb { void call(); }
    enum Mode { A }
}
";
    assert_eq!(
        outline("java", source),
        "1-10 class App
  2-2 App()
  3-7 run()
  8-8 interface Cb
  9-9 enum Mode
"
    );
    assert_eq!(at("java", source, 5), "class App › run()");
}

#[test]
fn kotlin_classes_objects_and_funs() {
    let source = "class Repo(val db: Db) {
    fun find(id: Int): User? {
        return db.get(id)
    }
    companion object {
        fun create() = Repo(Db())
    }
}
interface Api {
}
object Main {
    fun main() {}
}
";
    assert_eq!(
        outline("kotlin", source),
        "1-8 class Repo
  2-4 fun find
  5-7 companion object
    6-6 fun create
9-10 interface Api
11-13 object Main
  12-12 fun main
"
    );
}

#[test]
fn swift_kinds_come_from_the_declaration() {
    let source = "struct Point {
    var x: Int
    func moved() -> Point {
        return self
    }
}
extension Point: Equatable {
    init() {}
}
protocol Shape {
    func area() -> Double
}
";
    assert_eq!(
        outline("swift", source),
        "1-6 struct Point
  3-5 func moved
7-9 extension Point
  8-8 init
10-12 protocol Shape
"
    );
}

#[test]
fn labels_are_one_line_with_spaces_collapsed() {
    let source = "impl<T>   Trait\n    for Type<T> {\n    fn  a() {}\n}\n";
    assert_eq!(outline("rust", source), "1-4 impl Trait\n  3-3 fn a\n");
}

#[test]
fn a_language_without_a_scope_query_has_none() {
    let toml = GRAMMARS.named("toml").expect("toml");
    assert!(!toml.has_scopes());
    let text = Text::new(b"[a]\nb = 1\n".to_vec());
    assert!(
        parse(toml, text.bytes())
            .expect("parses")
            .scopes(&text)
            .is_none()
    );
}

/// Preparing a file's highlighting leaves a parsed side's definitions to
/// their own step, found from the tree it holds; a side with nothing to
/// look in needs no step.
#[test]
fn a_parsed_sides_scopes_are_a_step_after_its_highlighting() {
    let text = Text::new(RUST.into());
    let side = Source {
        path: b"a.rs",
        text: &text,
        id: None,
    };
    let syntax = FileSyntax::new(&GRAMMARS, [side, side], &[], None, false);
    assert_eq!(syntax.scopes(), None);
    syntax.find_scopes(&text, &text);
    let fresh = scopes_of("rust", RUST);
    assert_eq!(syntax.scopes(), Some([Some(&fresh), Some(&fresh)]));

    let toml = Text::new(b"[a]\nb = 1\n".to_vec());
    let side = Source {
        path: b"Cargo.toml",
        text: &toml,
        id: None,
    };
    let syntax = FileSyntax::new(&GRAMMARS, [side, side], &[], None, false);
    assert_eq!(syntax.scopes(), Some([None, None]));
}

/// A large side's definitions go to the disk cache with its spans, so a
/// hit, which parses nothing, still has them, without a step after.
#[test]
fn a_cached_side_keeps_its_scopes() {
    let mut source = String::from("impl A {\n");
    while source.len() < CACHE_MIN_BYTES {
        source.push_str("    fn f() {\n        x();\n    }\n");
    }
    source.push_str("}\n");
    let text = Text::new(source.into());
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = cache::Cache::new(dir.path().to_owned(), 1 << 30);
    let id = crate::git::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").ok();
    let side = Source {
        path: b"a.rs",
        text: &text,
        id,
    };
    let fresh = scopes_of("rust", std::str::from_utf8(text.bytes()).expect("utf-8"));
    assert_eq!(fresh.at(2), ["impl A", "fn f"]);
    for store in [true, false] {
        let syntax = FileSyntax::new(&GRAMMARS, [side, side], &[], Some(&cache), store);
        assert_eq!(
            syntax.scopes(),
            Some([Some(&fresh), Some(&fresh)]),
            "store {store}"
        );
    }
    assert!(matches!(
        Highlighter::for_side(&GRAMMARS, side, Some(&cache), false),
        Some(Highlighter::Cached(cached)) if cached.1.is_some()
    ));
}

#[test]
fn scopes_roundtrip_through_their_encoding() {
    let scopes = scopes_of("rust", RUST);
    let mut bytes = Vec::new();
    scopes.encode(&mut bytes);
    let back = Scopes::decode(&mut &bytes[..]).expect("decodes");
    for line in 0..30 {
        assert_eq!(back.at(line), scopes.at(line), "line {line}");
    }
    assert!(Scopes::decode(&mut &bytes[..bytes.len() - 1]).is_none());
}
