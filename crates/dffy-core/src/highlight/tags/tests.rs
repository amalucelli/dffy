use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::LazyLock;

use crate::cache::{Cache, SymbolCache};
use crate::git::ObjectId;
use crate::highlight::{Grammar, LanguageSource, Registry};

use super::*;

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

/// Each definition as `first-last kind name`, 1-based, with ` in
/// container` for a method that has one.
fn outline(lang: &str, source: &str) -> String {
    let lang = GRAMMARS.named(lang).expect("a bundled language");
    let defs = definitions(lang, source.as_bytes())
        .expect("the query compiles")
        .expect("a tags query")
        .expect("parsed");
    let mut out = String::new();
    for d in defs.iter() {
        let _ = write!(
            out,
            "{}-{} {} {}",
            d.start + 1,
            d.end + 1,
            d.kind.name(),
            d.name
        );
        if let Some(container) = d.container {
            let _ = write!(out, " in {container}");
        }
        if let Some(original) = d.original {
            let _ = write!(out, " from {original}");
        }
        if let Some(returns) = d.returns {
            let _ = write!(out, " -> {returns}");
        }
        out.push('\n');
    }
    out
}

#[test]
fn every_bundled_tags_query_compiles() {
    let with: Vec<_> = GRAMMARS
        .languages()
        .iter()
        .filter_map(|l| {
            let query = l.tags_query()?.expect("compiles");
            assert!(
                query.roots.is_some(),
                "{}: a pattern's first node isn't named",
                l.name()
            );
            Some(l.name())
        })
        .collect();
    assert_eq!(
        with,
        [
            "bash",
            "c",
            "cpp",
            "csharp",
            "elixir",
            "go",
            "java",
            "javascript",
            "kotlin",
            "lua",
            "php",
            "python",
            "ruby",
            "rust",
            "scala",
            "swift",
            "tsx",
            "typescript",
            "zig",
        ]
    );
}

/// Kernel-style C: an include guard around everything, definitions made by
/// macros in the shapes the grammar parses them (a call, a declaration,
/// two declarations fused into one, a function whose body is a separate
/// statement), and what only refers to a definition: a prototype, an
/// `extern`, a tagged type without a body, `EXPORT_SYMBOL`.
const C: &str = r#"#ifndef _FIXTURE_H
#define _FIXTURE_H
#include <linux/syscalls.h>

#define MAX_FOO 10
#define min(a, b) ((a) < (b) ? (a) : (b))
#define DEFINE_THING(name) struct thing name = { 0 }

static DEFINE_MUTEX(foo_lock);
DEFINE_SPINLOCK(bar_lock);
static DEFINE_PER_CPU(int, counter);
DEFINE_PER_CPU(struct foo, foo_pcpu);

SYSCALL_DEFINE3(openat, int, dfd, const char __user *, filename, int, flags)
{
	DEFINE_WAIT(wait);
	return do_sys_open(dfd, filename, flags, 0);
}

SYSCALL_DEFINE0(getpid)
{
	return 1;
}

COMPAT_SYSCALL_DEFINE2(foo, int, a, int, b)
{
	return 0;
}

TRACE_EVENT(sched_switch,
	TP_PROTO(bool preempt, struct task_struct *prev),
	TP_ARGS(preempt, prev),
	TP_printk("prev_comm=%s (%d)", __entry->prev_comm, 1)
);

DEFINE_EVENT(sched_wakeup_template, sched_wakeup,
	TP_PROTO(struct task_struct *p),
	TP_ARGS(p));

enum color { RED, GREEN = 2, };
typedef unsigned long ulong_t;
typedef struct { int a; } anon_t;
typedef int (*fn_t)(int);
struct list_head {
	struct list_head *next, *prev;
};
struct fwd;
union u { int a; float b; };
int proto(int a);
extern int y;
static inline __alloc_size(1, 2) void *kmalloc_array(size_t n, size_t size, gfp_t flags)
{
	return 0;
}
EXPORT_SYMBOL(kmalloc_array);
static int __init foo_init(void)
{
	struct local { int x; };
	return 0;
}
struct ring { DEFINE_DMA_UNMAP_ADDR(mapping); DEFINE_DMA_UNMAP_LEN(len); };
static DEFINE_PER_CPU(int, a1);
SYSCALL_DEFINE1(foo1, int, x)
{
	return x;
}
__visible DEFINE_IDTENTRY(exc_z)
{
}
TRACE_EVENT_FLAGS(sys_enter_x, 0);
TRACE_EVENT_PERF_PERM(x_event, 0);
TRACE_EVENT_NOP(nop_event, TP_PROTO(int a));
DEFINE_GUARD(mutex, struct mutex *, mutex_lock(_T), mutex_unlock(_T));
DEFINE_LOCK_GUARD_1(spinlock, spinlock_t, spin_lock(_T->lock), spin_unlock(_T->lock));
DEFINE_CLASS(fd, struct fd, fdput(_T), fdget(fd), int fd);
DEFINE_FREE(kfree, void *, if (_T) kfree(_T));
#if DEFINE_CHECK(cond)
#endif
static DEFINE_PER_CPU(int, arr[4]);
static DEFINE_MUTEX(/* the lock */ commented);
int (*get_handler(int n))(int)
{
	return 0;
}
typedef int (fn2_t)(int);
static int probe(void)
{
	DEFINE_WAIT(wait2);
#ifdef CONFIG_A
	if (a) {
#else
	if (b) {
#endif
		return 0;
	}
}
DEFINE_MUTEX(after_probe);
#endif
"#;

#[test]
fn c_definitions_include_those_made_by_macros() {
    assert_eq!(
        outline("c", C),
        "\
2-2 macro _FIXTURE_H
5-5 macro MAX_FOO
6-6 macro min
7-7 macro DEFINE_THING
9-9 variable foo_lock
10-10 variable bar_lock
11-11 variable counter
12-12 variable foo_pcpu
14-18 function sys_openat
20-23 function sys_getpid
25-28 function compat_sys_foo
30-34 function trace_sched_switch
36-38 function trace_sched_wakeup
40-40 enum color
40-40 enumerator RED
40-40 enumerator GREEN
41-41 type ulong_t
42-42 type anon_t
42-42 field a in anon_t
43-43 type fn_t
44-46 struct list_head
45-45 field next in list_head
45-45 field prev in list_head
48-48 union u
48-48 field a in u
48-48 field b in u
51-54 function kmalloc_array
56-60 function foo_init
61-61 struct ring
62-62 variable a1
63-66 function sys_foo1
67-69 function exc_z
76-76 function __free_kfree
79-79 variable arr
80-80 variable commented
81-84 function get_handler
85-85 type fn2_t
97-97 variable after_probe
"
    );
}

/// Items at the top, in a module and in impls and traits, a trait's
/// signatures included, fields, variants and `pub use` re-exports; a
/// function inside a body is local, and a private `use` imports.
const RUST: &str = r#"mod nested {
    pub fn inner() {}
}
mod file;
pub struct Point<T> { x: T }
enum Shape { Circle, Square }
union Bits { i: u32, f: f32 }
pub trait Draw {
    fn draw(&self);
    fn name(&self) -> &str {
        "shape"
    }
}
type Result<T> = std::result::Result<T, Error>;
const LIMIT: usize = 10;
static COUNT: AtomicUsize = AtomicUsize::new(0);
macro_rules! square {
    ($x:expr) => { $x * $x };
}
impl<T> Point<T> {
    pub fn new(x: T) -> Self {
        fn helper() {}
        Self { x }
    }
}
impl Draw for crate::shapes::Circle {
    fn draw(&self) {}
}
impl<T: Draw> Draw for &T {
    fn draw(&self) {}
}
fn main() {}
trait Limits {
    const MAX: usize;
    const MIN: usize = 0;
}
impl Limits for Point<u8> {
    const MAX: usize = 9;
    type Item = u8;
}
const _: () = ();
extern "C" {
    static ERRNO: i32;
}
pub use crate::shapes::{self, Circle, Square as Sq};
use crate::private::Hidden;
"#;

#[test]
fn rust_methods_carry_the_type_they_are_on() {
    assert_eq!(
        outline("rust", RUST),
        "\
1-3 module nested
2-2 function inner
4-4 module file
5-5 struct Point
5-5 field x in Point
6-6 enum Shape
6-6 enumerator Circle in Shape
6-6 enumerator Square in Shape
7-7 union Bits
7-7 field i in Bits
7-7 field f in Bits
8-13 trait Draw
9-9 method draw in Draw
10-12 method name in Draw
14-14 type Result
15-15 constant LIMIT
16-16 static COUNT
17-19 macro square
21-24 method new in Point -> Point
27-27 method draw in Circle
30-30 method draw
32-32 function main
33-36 trait Limits
34-34 constant MAX in Limits
35-35 constant MIN in Limits
38-38 constant MAX in Point
39-39 type Item in Point
45-45 reexport Circle
45-45 reexport Sq from Square
"
    );
}

/// The fixture of each language beyond C and Rust, by its name: a file of
/// `tags/fixtures/`.
const FIXTURES: &[(&str, &str)] = &[
    ("typescript", include_str!("fixtures/fixture.ts")),
    ("javascript", include_str!("fixtures/fixture.js")),
    ("python", include_str!("fixtures/fixture.py")),
    ("go", include_str!("fixtures/fixture.go")),
    ("java", include_str!("fixtures/fixture.java")),
    ("cpp", include_str!("fixtures/fixture.cpp")),
    ("csharp", include_str!("fixtures/fixture.cs")),
    ("kotlin", include_str!("fixtures/fixture.kt")),
    ("swift", include_str!("fixtures/fixture.swift")),
    ("ruby", include_str!("fixtures/fixture.rb")),
    ("php", include_str!("fixtures/fixture.php")),
    ("scala", include_str!("fixtures/fixture.scala")),
    ("zig", include_str!("fixtures/fixture.zig")),
    ("lua", include_str!("fixtures/fixture.lua")),
    ("elixir", include_str!("fixtures/fixture.ex")),
    ("bash", include_str!("fixtures/fixture.sh")),
];

fn fixture(lang: &str) -> &'static str {
    FIXTURES
        .iter()
        .find(|(name, _)| *name == lang)
        .map(|(_, source)| *source)
        .expect("a fixture")
}

/// Classes with their methods, overloads and abstract members that only
/// declare, interfaces, aliases, enums with their members, namespaces,
/// and variables at module scope only; the same in TSX.
#[test]
fn typescript_definitions_leave_out_signatures_and_locals() {
    let want = "\
2-5 function top
6-6 function gen
9-9 function over
10-22 class Point
11-11 field x in Point
12-12 method onClick in Point
16-18 method len in Point
20-20 method move in Point
23-26 class Shape2
24-24 method area in Shape2
25-25 method describe in Shape2
27-29 interface Shape
28-28 method area in Shape
30-30 type Id
31-31 enum Color
31-31 enumerator Red in Color
31-31 enumerator Green in Color
32-34 module Geo
33-33 function dist
38-38 constant LIMIT
38-38 constant OTHER
39-39 function handler
40-40 function fn
41-41 variable counter
49-49 constant obj
50-50 reexport fmt from format
50-50 reexport helper
52-52 function main
54-57 class Widget
55-55 field name in Widget
56-56 method onClick in Widget
";
    assert_eq!(outline("typescript", fixture("typescript")), want);
    assert_eq!(outline("tsx", fixture("typescript")), want);
}

/// Class fields holding a function are methods, JSX doesn't hide what's
/// around it, and functions assigned to `exports` are the module's.
#[test]
fn javascript_definitions_include_exports_and_jsx_components() {
    assert_eq!(
        outline("javascript", fixture("javascript")),
        "\
2-4 function top
5-5 function gen
6-15 class Widget
7-7 field count in Widget
8-8 method handle in Widget
12-14 method render in Widget
16-16 constant LIMIT
17-17 function App
18-18 variable legacy
19-19 function make
20-20 function other
"
    );
}

/// Methods, class attributes and what `__init__` assigns to `self` in
/// their class, module-level names with capitals as constants, what a
/// block under `if` or `try` at module scope defines, and relative imports
/// as re-exports; a function's locals are left out.
#[test]
fn python_definitions_keep_module_and_class_scope() {
    assert_eq!(
        outline("python", fixture("python")),
        "\
2-2 reexport helper
5-5 constant LIMIT
6-6 variable name
7-7 variable a
7-7 variable b
8-8 variable __all__
11-15 function top
19-20 function fetch
23-37 class Point
25-25 field origin in Point
26-26 field count in Point
28-29 method __init__ in Point
29-29 field x in Point
32-33 method length in Point
35-37 class Inner
36-37 method deep in Inner
41-42 function guarded
47-47 variable FALLBACK
49-49 type Alias
"
    );
}

/// Methods carry their receiver's type through a pointer and type
/// arguments, types are structs or interfaces by their declaration, and
/// grouped constants, variables and types each count; `init` doesn't.
#[test]
fn go_methods_carry_their_receivers_type() {
    assert_eq!(
        outline("go", fixture("go")),
        "\
5-5 constant Limit
8-8 constant Red
9-9 constant Green
12-12 variable Origin
15-15 variable count
16-16 variable debug
19-21 struct Point
20-20 field X in Point
20-20 field Y in Point
23-25 interface Shape
24-24 method Area in Shape
27-27 type Color
29-29 type Alias
32-32 type Pair
33-33 struct Empty
36-40 function New -> Point
42-44 method Move in Point
46-48 method First in Pair -> T
50-52 function Max -> T
"
    );
}

/// Methods (abstract, interface and annotation ones included), fields,
/// `static final` constants, enum constants and record components in
/// their type; constructors and a method body's anonymous class are left
/// out.
#[test]
fn java_members_carry_their_type() {
    assert_eq!(
        outline("java", fixture("java")),
        "\
5-25 class Point
6-6 constant LIMIT in Point
7-7 field x in Point
13-18 method area in Point
20-20 method draw in Point
22-24 class Inner
23-23 method deep in Inner
27-33 interface Shape
28-28 method area in Shape
30-32 method name in Shape
35-41 enum Color
36-36 enumerator RED in Color
36-36 enumerator GREEN in Color
38-40 method next in Color -> Color
43-47 class Pair
43-43 field a in Pair
43-43 field b in Pair
44-46 method sum in Pair
49-51 interface Marker
50-50 method value in Marker
"
    );
}

/// Methods defined in their class or outside it under a qualified name,
/// pure virtual ones, fields, namespaces, templates and namespace-scope
/// constants; other declarations in a class, prototypes, operators and
/// destructors are left out.
#[test]
fn cpp_methods_carry_their_class_in_or_out_of_it() {
    assert_eq!(
        outline("cpp", fixture("cpp")),
        "\
2-2 macro SQUARE
4-49 module geo
6-6 constant kLimit
8-15 class Point
11-11 method area in Point
12-12 method draw in Point
14-14 field x in Point
17-20 struct Pair
18-18 field a in Pair
18-18 field b in Pair
19-19 method sum in Pair
22-25 method area2 in Point
34-36 function max -> T
40-40 enum Color
40-40 enumerator Red
40-40 enumerator Green
42-42 type Id
43-43 type ulong_t
45-47 module detail
46-46 function helper
52-55 class Box
53-53 method get in Box -> T
54-54 field value in Box
57-57 method reset in Pair
59-63 struct Shape
60-60 method area in Shape
62-62 field sides in Shape
"
    );
}

/// Methods with a body (an arrow's included), abstract and interface ones,
/// properties, fields, events, `const` fields as constants and record
/// parameters in their type, and namespaces by their last part; partial
/// methods without a body and constructors are left out.
#[test]
fn csharp_members_carry_their_type() {
    assert_eq!(
        outline("csharp", fixture("csharp")),
        "\
3-42 module Geo
5-25 class Point
7-7 constant Limit in Point
8-8 field x in Point
9-9 field X in Point
13-13 method Area in Point
15-15 method Draw in Point
19-19 field Moved in Point
21-24 class Inner
23-23 method Deep in Inner
27-30 interface IShape
29-29 method Area in IShape
32-35 struct Pair
34-34 method Sum in Pair
37-37 enum Color
37-37 enumerator Red in Color
37-37 enumerator Green in Color
39-39 class Person
39-39 field Name in Person
41-41 type Handler
44-44 module Other
46-49 class Ext
48-48 method Twice in Ext
"
    );
}

/// Functions in classes, companion objects and objects (abstract and
/// interface ones included) and properties, a primary constructor's too,
/// carry their type, an extension function its receiver's; interfaces and
/// enums are told apart from classes, and top-level `const val`s are
/// constants.
#[test]
fn kotlin_functions_carry_their_type_or_receiver() {
    assert_eq!(
        outline("kotlin", fixture("kotlin")),
        "\
5-5 constant LIMIT
6-6 variable origin
7-7 variable counter
9-13 function top
15-15 method twice in Int
17-31 class Point
17-17 field x in Point
18-18 field size in Point
22-22 method area in Point
25-25 method origin in Point
28-30 class Inner
29-29 method deep in Inner
33-37 interface Shape
34-34 method area in Shape
36-36 method name in Shape
39-41 class Base
40-40 method draw in Base
43-43 class Pair
43-43 field a in Pair
43-43 field b in Pair
45-45 enum Color
45-45 enumerator RED in Color
45-45 enumerator GREEN in Color
47-49 class Registry
48-48 method register in Registry
51-51 type Id
"
    );
}

/// Classes, structs, enums and actors told apart by their keyword, an
/// extension's methods on the type it extends, enum cases, and top-level
/// `let`s as constants, and protocol requirements in their protocol;
/// initializers are left out.
#[test]
fn swift_types_are_told_apart_by_their_keyword() {
    assert_eq!(
        outline("swift", fixture("swift")),
        "\
3-3 constant limit
4-4 variable counter
6-10 function top
12-25 class Point
13-13 field x in Point
14-14 field origin in Point
18-20 method area in Point
22-24 class Inner
23-23 method deep in Inner
27-29 struct Pair
28-28 method sum in Pair
31-33 interface Shape
32-32 method area in Shape
36-36 method moved in Point -> Point
39-44 enum Color
40-40 enumerator red in Color
40-40 enumerator green in Color
41-41 enumerator blue in Color
43-43 method next in Color -> Color
46-48 class Counter
47-47 method increment in Counter
50-50 type Id
"
    );
}

fn c_with_tags(tags: &'static str) -> Registry {
    Registry::new([Grammar {
        name: Cow::Borrowed("c"),
        file_names: Cow::Owned(Vec::new()),
        extensions: Cow::Owned(vec![Cow::Borrowed("c")]),
        interpreters: Cow::Owned(Vec::new()),
        language: LanguageSource::Fn(|| tree_sitter_c::LANGUAGE.into()),
        highlights: Cow::Owned(Vec::new()),
        scopes: Cow::Owned(Vec::new()),
        tags: Cow::Owned(vec![Cow::Borrowed(tags)]),
        locals: Cow::Owned(Vec::new()),
        indentation_is_syntax: false,
    }])
}

#[test]
fn a_tags_query_that_fails_to_compile_is_an_error() {
    for tags in [
        "(no_such_node) @definition.function",
        "(function_definition) @definition.function",
        "(preproc_def name: (identifier) @name) @definition.no_such_kind",
    ] {
        let grammars = c_with_tags(tags);
        let lang = grammars.named("c").expect("registered");
        let err = definitions(lang, b"int x;").expect_err(tags);
        assert!(
            err.to_string()
                .starts_with("the c definitions query doesn't compile: "),
            "{err}"
        );
    }
}

/// Re-exports with their original names among them, and without, and
/// functions with a return type.
#[test]
fn definitions_round_trip_through_their_encoding() {
    let (mut reexports, mut returns) = ((0, 0), 0);
    for &(lang, source) in [("c", C), ("rust", RUST)].iter().chain(FIXTURES) {
        let lang = GRAMMARS.named(lang).expect("bundled");
        let defs = definitions(lang, source.as_bytes())
            .expect("compiles")
            .expect("a tags query")
            .expect("parsed");
        returns += defs.iter().filter(|d| d.returns.is_some()).count();
        for d in defs.iter().filter(|d| d.kind == DefinitionKind::Reexport) {
            if d.original.is_some() {
                reexports.0 += 1;
            } else {
                reexports.1 += 1;
            }
        }
        let bytes = defs.encode();
        assert_eq!(Definitions::decode(&bytes), Some(defs));
        for len in 0..bytes.len() {
            assert_eq!(Definitions::decode(&bytes[..len]), None, "cut at {len}");
        }
        let mut longer = bytes;
        longer.push(0);
        assert_eq!(Definitions::decode(&longer), None);
    }
    assert!(reexports.0 > 0 && reexports.1 > 0, "{reexports:?}");
    assert!(returns > 0);
}

#[test]
fn blob_definitions_are_cached_outside_the_shared_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shared = Cache::new(dir.path().to_owned(), 1 << 20);
    let symbols = shared.symbols();
    let lang = GRAMMARS.named("c").expect("bundled");
    let id = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").expect("hex");
    let size = C.len() as u64;
    let first = blob_definitions(lang, id, size, &symbols, || Some(C.as_bytes().to_vec()))
        .expect("compiles")
        .expect("a tags query")
        .expect("parsed");
    let again = blob_definitions(lang, id, size, &symbols, || panic!("read from the cache"))
        .expect("compiles");
    assert_eq!(again, Some(Ok(first)));
    assert_eq!(shared.usage().expect("readable"), (0, 0));
    assert_eq!(symbols.usage().expect("readable").1, 1);
}

#[test]
fn a_blob_too_large_to_read_is_never_loaded_and_stays_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let symbols = Cache::new(dir.path().to_owned(), 1 << 20).symbols();
    let lang = GRAMMARS.named("c").expect("bundled");
    let id = ObjectId::from_hex(b"89abcdef0123456789abcdef0123456789abcdef").expect("hex");
    let too_large = MAX_TAGS_BYTES as u64 + 1;
    for _ in 0..2 {
        let found = blob_definitions(lang, id, too_large, &symbols, || panic!("not loaded"));
        assert_eq!(found, Ok(Some(Err(Skipped::TooLarge))));
    }
    assert_eq!(symbols.usage().expect("readable").1, 1);
    let rust_free = GRAMMARS.named("json").expect("bundled");
    let none = blob_definitions(rust_free, id, 10, &symbols, || panic!("not loaded"));
    assert_eq!(none, Ok(None));
}

/// Blobs share the symbol cache's packs, so many blobs take a fixed number
/// of entries, and each is read back from its pack.
#[test]
fn blobs_definitions_are_packed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let symbols = Cache::new(dir.path().to_owned(), 1 << 30).symbols();
    let lang = GRAMMARS.named("c").expect("bundled");
    let ids: Vec<_> = (0..2 * PACKS)
        .map(|i| cache::digest(&i.to_le_bytes()))
        .collect();
    for (i, id) in ids.iter().enumerate() {
        let source = format!("int f{i}(void) {{ return 0; }}\n");
        let size = source.len() as u64;
        blob_definitions(lang, *id, size, &symbols, || Some(source.into_bytes()))
            .expect("compiles");
    }
    let (_, entries) = symbols.usage().expect("readable");
    assert!(entries <= PACKS, "{entries} entries");
    for (i, id) in ids.iter().enumerate() {
        let defs = blob_definitions(lang, *id, 1, &symbols, || panic!("read from its pack"))
            .expect("compiles")
            .expect("a tags query")
            .expect("parsed");
        assert_eq!(defs.iter().next().map(|d| d.name), Some(&*format!("f{i}")));
    }
}

/// A budget whose packs keep a few entries of a kilobyte each.
const SMALL: u64 = PACKS as u64 * 10_000;

fn symbols(budget: u64) -> (tempfile::TempDir, SymbolCache) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(dir.path().to_owned(), budget).symbols();
    (dir, cache)
}

fn key(i: usize) -> ObjectId {
    cache::digest(&i.to_le_bytes())
}

/// Which of the first `n` keys pack 0 holds.
fn kept(cache: &SymbolCache, n: usize) -> Vec<usize> {
    let mut pack = Pack::read_index(cache, 0);
    (0..n).filter(|&i| pack.get(&key(i)).is_some()).collect()
}

/// A pack past its share of the budget drops its least recently used
/// entries first: an entry read since the pack was read moves to the back
/// when it's written again. With the other packs empty it borrows its even
/// share again from the pool.
#[test]
fn a_full_pack_drops_its_least_recently_used_entries() {
    let (_dir, cache) = symbols(SMALL);
    let mut pack = Pack::read_index(&cache, 0);
    for i in 0..10 {
        pack.insert(key(i), vec![1; 1000]);
    }
    pack.write(&cache);
    assert_eq!(kept(&cache, 10), [3, 4, 5, 6, 7, 8, 9]);
    let mut pack = Pack::read_index(&cache, 0);
    assert!(pack.get(&key(6)).is_some());
    pack.insert(key(10), vec![1; 1000]);
    pack.write(&cache);
    assert_eq!(kept(&cache, 11), [4, 5, 6, 7, 8, 9, 10]);
    assert!(cache.usage().expect("readable").0 <= 2 * cache.budget() / PACKS as u64);
}

/// A pack takes more than its even share only from what the other packs
/// left of the pool: with them full it keeps its even share, and the
/// packs together stay within the budget.
#[test]
fn a_pack_takes_more_than_its_share_only_from_what_others_leave() {
    let (_dir, cache) = symbols(SMALL);
    for p in 1..PACKS {
        let mut pack = Pack::read_index(&cache, p);
        for i in 0..9 {
            pack.insert(key(p * 100 + i), vec![1; 1000]);
        }
        pack.write(&cache);
    }
    let mut pack = Pack::read_index(&cache, 0);
    for i in 0..10 {
        pack.insert(key(i), vec![1; 1000]);
    }
    pack.write(&cache);
    assert_eq!(kept(&cache, 10), [7, 8, 9]);
    let (allocated, _) = cache.usage().expect("readable");
    assert!(
        allocated <= cache.budget(),
        "{allocated} > {}",
        cache.budget()
    );
}

/// A pack written keeps what another writer stored in it since it was
/// read.
#[test]
fn a_pack_keeps_what_another_writer_added_since_it_was_read() {
    let (_dir, cache) = symbols(1 << 30);
    let (mut a, mut b) = (Pack::read_index(&cache, 0), Pack::read_index(&cache, 0));
    a.insert(key(1), vec![1]);
    a.write(&cache);
    b.insert(key(2), vec![2]);
    b.write(&cache);
    assert_eq!(kept(&cache, 3), [1, 2]);
}

/// An entry larger than a pack's share isn't kept, and doesn't push out
/// the others.
#[test]
fn an_entry_larger_than_a_packs_share_is_not_kept() {
    let (_dir, cache) = symbols(SMALL);
    let mut pack = Pack::read_index(&cache, 0);
    pack.insert(key(0), vec![1; 100]);
    pack.insert(key(1), vec![1; 20_000]);
    pack.write(&cache);
    assert_eq!(kept(&cache, 2), [0]);
}

/// The kernel's attribute macros that take arguments, written between a
/// function's storage class and its type, would leave the grammar no
/// function to find.
#[test]
fn attribute_macros_before_a_function_leave_it_defined() {
    let source = "static __printf(2, 3) int note(int a, const char *fmt, ...)
{
\treturn 0;
}
static __always_inline __alloc_size(1) void *grab(size_t size)
{
\treturn 0;
}
static inline __realloc_size(2) void *regrab(void *p, size_t size)
{
\treturn p;
}
#define __printf(a, b) __attribute__((format(printf, a, b)))
";
    assert_eq!(
        outline("c", source),
        "1-4 function note\n5-8 function grab\n9-12 function regrab\n13-13 macro __printf\n"
    );
}

/// An attribute macro's name whose parenthesis seems to close far away, as
/// in a comment, isn't read as spaces over the code up to it.
#[test]
fn attribute_macro_arguments_are_bounded() {
    let source = "/* see __printf( */
struct kept {
\tint a;
};
#define END )
";
    assert_eq!(
        outline("c", source),
        "2-4 struct kept\n3-3 field a in kept\n5-5 macro END\n"
    );
}

/// The cached form's numbers for each kind. Renumbering one reads old
/// entries as the wrong kind.
#[test]
fn definition_kinds_keep_their_numbers() {
    let numbered: Vec<_> = DefinitionKind::ALL
        .iter()
        .map(|k| (*k as u8, k.name()))
        .collect();
    assert_eq!(
        numbered,
        [
            (0, "function"),
            (1, "method"),
            (2, "macro"),
            (3, "type"),
            (4, "struct"),
            (5, "union"),
            (6, "enum"),
            (7, "enumerator"),
            (8, "trait"),
            (9, "constant"),
            (10, "static"),
            (11, "variable"),
            (12, "module"),
            (13, "class"),
            (14, "interface"),
            (15, "field"),
            (16, "reexport"),
            (17, "local"),
            (18, "parameter"),
            (19, "import"),
        ]
    );
}

/// What the fixtures yield is part of what a cached entry means: when it
/// changes, bump `TAGS_VERSION` so entries made before aren't read, then
/// update the digest here. A new language's fixture changes the digest
/// without a bump, as no entry of its was made before.
#[test]
fn tags_version_follows_what_extraction_finds() {
    let mut yielded = Vec::new();
    for &(lang, source) in [("c", C), ("rust", RUST)].iter().chain(FIXTURES) {
        yielded.extend_from_slice(outline(lang, source).as_bytes());
        let lang = GRAMMARS.named(lang).expect("bundled");
        let defs = definitions(lang, source.as_bytes())
            .expect("compiles")
            .expect("a tags query")
            .expect("parsed");
        yielded.extend_from_slice(&defs.encode());
    }
    let digest = cache::digest(&yielded).to_hex().to_string();
    assert_eq!(
        (TAGS_VERSION, digest.as_str()),
        (8, "e584a8f5937998d9b6d47a4e926c88d145b550a5"),
        "the fixtures yield something else: bump TAGS_VERSION and update the digest"
    );
}

/// A class's and a module's methods, `self.` ones and those in
/// `class << self` included, constants, `attr_*` readers and the instance
/// variables `initialize` assigns, named without `:` or `@`.
#[test]
fn ruby_members_carry_their_class_or_module() {
    assert_eq!(
        outline("ruby", fixture("ruby")),
        "\
3-3 constant LIMIT
5-31 module Geo
6-6 constant VERSION in Geo
8-28 class Point
9-9 field x in Point
9-9 field y in Point
10-10 constant ORIGIN in Point
12-14 method initialize in Point
13-13 field x in Point
16-18 method origin in Point
20-23 method area in Point
26-26 method build in Point
30-30 method helper in Geo
33-35 function top
39-55 class Account
40-42 method empty in Account
44-44 method save in Account
46-48 method name in Account
50-50 method secret in Account
52-54 method cached in Account
57-59 module Tools
58-58 method util in Tools
"
    );
}

/// Methods (abstract and interface ones too), properties without `$`,
/// class constants and enum cases in their type, and the namespace by its
/// last part; a constructor and a function inside a function are left out.
#[test]
fn php_members_carry_their_type() {
    assert_eq!(
        outline("php", fixture("php")),
        "\
2-2 module Models
6-6 constant LIMIT
8-11 function top
13-15 interface Shape
14-14 method area in Shape
17-19 trait Greets
18-18 method greet in Greets
21-33 class Point
22-22 constant ORIGIN in Point
23-23 field x in Point
24-24 field count in Point
28-30 method area in Point
32-32 method draw in Point
35-40 enum Color
36-36 enumerator Red in Color
37-37 enumerator Green in Color
39-39 method label in Color
"
    );
}

/// Members of classes, objects, traits and enums, a case class's
/// parameters, an extension's methods on the type it extends, and
/// top-level `val`s and givens.
#[test]
fn scala_members_carry_their_type() {
    assert_eq!(
        outline("scala", fixture("scala")),
        "\
5-5 variable Limit
7-10 function top
11-14 trait Shape
12-12 method area in Shape
13-13 method name in Shape
15-15 class Base
17-20 class Point
17-17 field x in Point
17-17 field y in Point
18-18 field size in Point
19-19 method area in Point
21-23 class Point
22-22 method origin in Point -> Point
24-26 enum Color
25-25 enumerator Red in Color
25-25 enumerator Green in Color
27-27 type Id
29-29 variable ordering
32-32 method moved in Point -> Point
"
    );
}

/// A container is named by the constant it's assigned to: its fields,
/// functions and declarations carry that name, an enum's fields are its
/// values; a test and a function's locals are left out.
#[test]
fn zig_containers_are_named_by_their_declaration() {
    assert_eq!(
        outline("zig", fixture("zig")),
        "\
1-1 constant std
3-3 constant limit
4-4 variable counter
6-9 function top
11-20 struct Point
12-12 field x in Point
13-13 field y in Point
15-15 constant origin in Point
17-19 method area in Point
22-29 enum Color
23-23 enumerator red in Color
24-24 enumerator green in Color
26-28 method next in Color -> Color
31-34 union Value
32-32 field int in Value
33-33 field none in Value
40-44 struct Outer
41-43 struct Inner
42-42 constant depth in Inner
"
    );
}

/// Functions and values stored in a table carry the table; file-scope
/// names count, global or local, and a function's locals don't.
#[test]
fn lua_table_members_carry_their_table() {
    assert_eq!(
        outline("lua", fixture("lua")),
        "\
1-1 variable util
3-3 variable LIMIT
4-4 variable counter
6-9 function top
11-11 function helper
13-13 variable M
15-15 field version in M
17-19 method area in M
21-22 method draw in M
24-24 method scale in M
"
    );
}

/// Each clause of a function, macros, guards, delegations, struct fields
/// and module attributes in their module, named by its last part; a
/// protocol is an interface, and the attributes Elixir reads are left out.
#[test]
fn elixir_definitions_are_calls_in_their_module() {
    assert_eq!(
        outline("elixir", fixture("elixir")),
        "\
1-23 module Point
3-3 constant origin in Point
5-5 field x in Point
5-5 field y in Point
7-7 method area in Point
9-9 method area in Point
11-14 method helper in Point
16-18 macro square in Point
20-20 method is_point in Point
22-22 method parse in Point
25-27 interface Shape
26-26 method area in Shape
30-30 method area in Shape
33-36 module Check
34-34 method valid in Check
35-35 method run in Check
"
    );
}

/// Functions, those under `if` included, and top-level variables, with
/// `readonly` ones as constants; a function inside another's body and a
/// heredoc's text are left out.
#[test]
fn bash_functions_and_top_level_variables() {
    assert_eq!(
        outline("bash", fixture("bash")),
        "\
4-4 variable LIMIT
5-5 constant ROOT
6-6 variable PATH
8-11 function usage
13-16 function build
19-19 function debug
"
    );
}

/// What a language's rules say is part of its cache key: the walk's
/// pruning, the attribute macros, the name suffixes and whether
/// constructors count.
#[test]
fn a_languages_rules_are_part_of_its_tags_identity() {
    assert!(rules_identity("c").contains("__alloc_size"));
    assert!(rules_identity("ruby").contains("'?'"));
    assert!(rules_identity("cpp").contains("Dropped"));
    assert!(rules_identity("go").contains("field_declaration_list"));
    assert_eq!(rules_identity("json"), "");
}

/// The kernel's attribute macros after an untagged member's body, which
/// the grammar takes for the member's name, name no field.
#[test]
fn attribute_macros_after_a_member_are_no_fields() {
    let source = "struct frame {
\tstruct {
\t\tint a;
\t} __packed;
\tunion { int b; } ____cacheline_aligned_in_smp;
\tint __pad;
};
";
    assert_eq!(
        outline("c", source),
        "1-7 struct frame\n6-6 field __pad in frame\n"
    );
}

/// A function's declared return type, by each grammar's form: past a
/// pointer, a wrapper, a second result and a quote, with `Self` as its type,
/// and none for a primitive.
#[test]
fn functions_carry_the_type_they_return() {
    let returned = |lang: &str, source: &str| -> Vec<String> {
        let lang = GRAMMARS.named(lang).expect("bundled");
        let defs = definitions(lang, source.as_bytes())
            .expect("compiles")
            .expect("a tags query")
            .expect("parsed");
        defs.iter()
            .filter_map(|d| Some(format!("{} {}", d.name, d.returns?)))
            .collect()
    };
    assert_eq!(
        returned(
            "go",
            "package p\n\nfunc NewFoo() (*Foo, error) { return nil, nil }\n"
        ),
        ["NewFoo Foo"]
    );
    assert_eq!(
        returned(
            "rust",
            "impl Foo {\n    fn new() -> Option<Self> { None }\n    fn len(&self) -> usize { 0 }\n}\n"
        ),
        ["new Foo"]
    );
    assert_eq!(
        returned(
            "typescript",
            "export function make(): Promise<Foo> { return x; }\nclass A {\n  get(): Bar { return y; }\n}\n"
        ),
        ["make Foo", "get Bar"]
    );
    assert_eq!(
        returned(
            "python",
            "def session() -> \"Session\":\n    pass\n\n\ndef find() -> Optional[Model]:\n    pass\n"
        ),
        ["session Session", "find Model"]
    );
    assert_eq!(
        returned(
            "kotlin",
            "fun Foo.build(): Bar = Bar()\nfun Foo.size() = 1\n"
        ),
        ["build Bar"]
    );
    assert_eq!(
        returned(
            "csharp",
            "class A {\n  public Foo Make() { return null; }\n}\n"
        ),
        ["Make Foo"]
    );
    assert_eq!(
        returned("swift", "func make() -> Foo { Foo() }\n"),
        ["make Foo"]
    );
}

/// Argument lists that never close cost a file a bounded number of reads:
/// each call's scan comes off the file's allowance, an attribute's off its
/// own.
#[test]
fn unclosed_macro_arguments_are_read_within_an_allowance() {
    let source = "DEFINE_MUTEX(".repeat(1_000);
    let (mut left, mut scans) = (8 * source.len(), 0);
    for at in source.match_indices('(').map(|(at, _)| at) {
        assert_eq!(macro_args(source.as_bytes(), at, &mut left), None);
        scans += usize::from(left > 0);
    }
    assert!(
        left == 0 && scans < 20,
        "{left} bytes left after {scans} scans"
    );
    let mut left = 64;
    let closed = macro_args(b"(a, b) c", 0, &mut left).map(|(args, close)| (args.len(), close));
    assert_eq!((closed, left), (Some((2, 5)), 58));
    assert_eq!(macro_args(b"(a, b)", 0, &mut 5), None, "closes past it");
}

/// Macros nested thousands deep are read on a request's 2 MB stack.
#[test]
fn deeply_nested_macros_are_read() {
    let source = format!("{}DEFINE_MUTEX(a);{}", "(".repeat(2_000), ")".repeat(2_000));
    let found = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let lang = GRAMMARS.named("c").expect("bundled");
            definitions(lang, source.as_bytes())
                .expect("compiles")
                .is_some()
        })
        .expect("spawns")
        .join()
        .expect("no overflow");
    assert!(found);
}

/// A header's key is none of a C or a C++ parse's, and no other file's is
/// changed by what decides a header's language.
#[test]
fn a_headers_key_is_its_own() {
    let (c, cpp) = (
        GRAMMARS.named("c").expect("c"),
        GRAMMARS.named("cpp").expect("cpp"),
    );
    let id = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").expect("hex");
    let header = pack_key(c, b"a.h", id);
    assert_ne!(header, pack_key(c, b"a.c", id));
    assert_ne!(header, pack_key(cpp, b"a.h", id));
    assert_eq!(pack_key(c, b"a.c", id), pack_key(c, b"", id));
    assert_eq!(pack_key(cpp, b"a.hpp", id), pack_key(cpp, b"", id));
}
