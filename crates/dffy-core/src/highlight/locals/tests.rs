use std::sync::LazyLock;

use crate::highlight::{Registry, local_at, parse_tree, receiver_at};

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

/// What the `nth` use (0-based) of `name` on line `line` (1-based) of
/// `source` resolves to: its kind and 1-based line, or `None`.
fn at(
    language: &str,
    source: &str,
    line: usize,
    name: &str,
    nth: usize,
) -> Option<(String, usize)> {
    let lang = GRAMMARS.named(language).expect("bundled");
    let tree = parse_tree(
        lang,
        source.as_bytes(),
        std::time::Duration::from_secs(5),
        None,
    )
    .expect("parses");
    let start: usize = source.lines().take(line - 1).map(|l| l.len() + 1).sum();
    let text = source.lines().nth(line - 1).expect("the line");
    let column = (text.match_indices(name))
        .filter(|(i, _)| {
            let b = text.as_bytes();
            (*i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
                && b.get(i + name.len())
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
        })
        .nth(nth)
        .expect("the use")
        .0;
    let found = local_at(
        lang,
        &tree,
        source.as_bytes(),
        start + column..start + column + name.len(),
        &|| false,
    )?;
    Some((found.kind.name().to_owned(), found.line + 1))
}

fn kind(k: &str, line: usize) -> (String, usize) {
    (k.to_owned(), line)
}

#[test]
fn every_bundled_locals_query_compiles() {
    let with: Vec<_> = GRAMMARS
        .languages()
        .iter()
        .filter(|l| l.has_locals())
        .map(|l| {
            assert!(
                l.locals_query().is_some(),
                "{}: the locals query compiles",
                l.name()
            );
            l.name()
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
            "zig"
        ]
    );
}

/// A parameter, a local of an inner block shadowing it, a member that
/// isn't a local, a file-scope variable, and a prototype that defines
/// nothing.
#[test]
fn c_names_resolve_to_parameters_locals_and_file_scope_variables() {
    let source = "static int count;
int proto(int a);
struct node { struct node *next; };
int walk(struct node *next, int n)
{
\tint total = n;
\tfor (int i = 0; i < n; i++) {
\t\tint n = i;
\t\ttotal += n + count;
\t}
\treturn total + next->next + proto(n);
}
";
    assert_eq!(at("c", source, 6, "n", 0), Some(kind("parameter", 4)));
    assert_eq!(at("c", source, 9, "n", 0), Some(kind("local", 8)));
    assert_eq!(at("c", source, 9, "total", 0), Some(kind("local", 6)));
    assert_eq!(at("c", source, 11, "next", 0), Some(kind("parameter", 4)));
    assert_eq!(at("c", source, 11, "next", 1), None, "a member");
    assert_eq!(at("c", source, 9, "count", 0), Some(kind("variable", 1)));
    assert_eq!(at("c", source, 11, "proto", 0), None, "a function");
}

/// A receiver, `:=` defining again what came before, a range's variable,
/// and a selector's field.
#[test]
fn go_names_resolve_to_receivers_and_short_declarations() {
    let source = "package p

func (p *Point) Move(dx int) int {
\tx := p.x + dx
\tfor i, v := range p.items {
\t\tx := x + i + v
\t\t_ = x
\t}
\treturn x
}
";
    assert_eq!(at("go", source, 4, "p", 0), Some(kind("parameter", 3)));
    assert_eq!(at("go", source, 4, "x", 1), None, "a field");
    assert_eq!(at("go", source, 6, "x", 1), Some(kind("local", 4)));
    assert_eq!(at("go", source, 7, "x", 0), Some(kind("local", 6)));
    assert_eq!(at("go", source, 6, "v", 0), Some(kind("local", 5)));
    assert_eq!(at("go", source, 9, "x", 0), Some(kind("local", 4)));
    assert_eq!(at("go", source, 4, "dx", 0), Some(kind("parameter", 3)));
}

/// Parameters, destructured ones too, `var` and a function hoisted, a
/// property that isn't a local, and a name only the module defines.
#[test]
fn javascript_and_typescript_names_resolve_to_parameters_and_hoisted_locals() {
    let js = "const top = 1;
function run(a, { b }, ...rest) {
\tlater();
\tconst c = a + b + rest.length + top;
\tvar d = c.size;
\tfunction later() { return d; }
\treturn [1].map((e) => e + c);
}
";
    assert_eq!(at("javascript", js, 4, "a", 0), Some(kind("parameter", 2)));
    assert_eq!(at("javascript", js, 4, "b", 0), Some(kind("parameter", 2)));
    assert_eq!(
        at("javascript", js, 4, "rest", 0),
        Some(kind("parameter", 2))
    );
    assert_eq!(at("javascript", js, 4, "top", 0), None, "the module's");
    assert_eq!(at("javascript", js, 3, "later", 0), Some(kind("local", 6)));
    assert_eq!(at("javascript", js, 6, "d", 0), Some(kind("local", 5)));
    assert_eq!(at("javascript", js, 7, "e", 1), Some(kind("parameter", 7)));
    assert_eq!(at("javascript", js, 5, "size", 0), None, "a property");
    let ts = "function area(p: Point, scale?: number): number {
\tconst w: number = p.w * (scale ?? 1);
\treturn w;
}
";
    assert_eq!(at("typescript", ts, 2, "p", 0), Some(kind("parameter", 1)));
    assert_eq!(
        at("typescript", ts, 2, "scale", 0),
        Some(kind("parameter", 1))
    );
    assert_eq!(at("tsx", ts, 3, "w", 0), Some(kind("local", 2)));
}

/// A parameter, a local assigned after its use in the function (Python's
/// function scope), a comprehension's variable, an attribute, and a class
/// attribute a method doesn't see.
#[test]
fn python_names_resolve_in_function_scope() {
    let source = "class Point:
    size = 1

    def area(self, scale=2):
        if scale:
            total = self.size * scale
        squares = [x * x for x in range(total)]
        return total + size
";
    assert_eq!(
        at("python", source, 6, "scale", 0),
        Some(kind("parameter", 4))
    );
    assert_eq!(
        at("python", source, 6, "self", 0),
        Some(kind("parameter", 4))
    );
    assert_eq!(at("python", source, 6, "size", 0), None, "an attribute");
    assert_eq!(at("python", source, 7, "total", 0), Some(kind("local", 6)));
    assert_eq!(at("python", source, 7, "x", 0), Some(kind("local", 7)));
    assert_eq!(at("python", source, 8, "size", 0), None, "the class's");
}

/// `let` shadowing what came before, a match arm's binding, a closure's
/// parameter, a unit variant that isn't a binding, and a field.
#[test]
fn rust_names_resolve_to_shadowing_lets_and_pattern_bindings() {
    let source = "fn run(items: &[u32], limit: Option<u32>) -> u32 {
    let total = 0;
    let total = total + 1;
    let extra = match limit {
        Some(cap) => cap + total,
        None => 0,
    };
    items.iter().map(|item| item + extra).sum::<u32>() + self.total
}
";
    assert_eq!(at("rust", source, 3, "total", 1), Some(kind("local", 2)));
    assert_eq!(at("rust", source, 5, "cap", 1), Some(kind("local", 5)));
    assert_eq!(at("rust", source, 5, "total", 0), Some(kind("local", 3)));
    assert_eq!(
        at("rust", source, 4, "limit", 0),
        Some(kind("parameter", 1))
    );
    assert_eq!(at("rust", source, 8, "item", 1), Some(kind("parameter", 8)));
    assert_eq!(
        at("rust", source, 8, "items", 0),
        Some(kind("parameter", 1))
    );
    assert_eq!(at("rust", source, 6, "None", 0), None, "a variant");
    assert_eq!(at("rust", source, 8, "total", 0), None, "a field");
}

/// Parameters, a local, a loop's and a catch's variables, a lambda's
/// parameter, and a field that isn't a local.
#[test]
fn java_names_resolve_to_parameters_and_locals() {
    let source = "class A {
  int f(String s, int n) {
    var x = s.length();
    for (int i = 0; i < n; i++) { x += i; }
    for (String t : list) { t.trim(); }
    try { } catch (Exception e) { e.getMessage(); }
    Runnable r = (y) -> y.z();
    return x + this.n;
  }
}
";
    assert_eq!(at("java", source, 3, "s", 0), Some(kind("parameter", 2)));
    assert_eq!(at("java", source, 4, "i", 2), Some(kind("local", 4)));
    assert_eq!(at("java", source, 4, "x", 0), Some(kind("local", 3)));
    assert_eq!(at("java", source, 5, "t", 1), Some(kind("local", 5)));
    assert_eq!(at("java", source, 6, "e", 1), Some(kind("parameter", 6)));
    assert_eq!(at("java", source, 7, "y", 1), Some(kind("parameter", 7)));
    assert_eq!(at("java", source, 8, "n", 0), None, "a field");
}

/// Parameters, `val` and `var`, a loop's and a lambda's variables, and a
/// member that isn't a local.
#[test]
fn kotlin_names_resolve_to_parameters_and_locals() {
    let source = "class A {
  fun f(s: String, n: Int): Int {
    val x = s.length
    var y = x + n
    for (i in xs) { y += i }
    xs.map { q -> q.w }
    return y + this.n
  }
}
";
    assert_eq!(at("kotlin", source, 3, "s", 0), Some(kind("parameter", 2)));
    assert_eq!(at("kotlin", source, 4, "x", 0), Some(kind("local", 3)));
    assert_eq!(at("kotlin", source, 5, "y", 0), Some(kind("local", 4)));
    assert_eq!(at("kotlin", source, 5, "i", 1), Some(kind("local", 5)));
    assert_eq!(at("kotlin", source, 6, "q", 1), Some(kind("parameter", 6)));
    assert_eq!(at("kotlin", source, 7, "n", 0), None, "a member");
}

/// Parameters, `var`, `foreach`, a catch's variable, a lambda's implicit
/// parameter, and a member that isn't a local.
#[test]
fn csharp_names_resolve_to_parameters_and_locals() {
    let source = "class A {
  int F(string s, int n) {
    var x = s.Length;
    foreach (var i in xs) { x += i; }
    try { } catch (Exception e) { e.ToString(); }
    Func<int, int> g = z => z + n;
    return x + this.n;
  }
}
";
    assert_eq!(at("csharp", source, 3, "s", 0), Some(kind("parameter", 2)));
    assert_eq!(at("csharp", source, 4, "i", 1), Some(kind("local", 4)));
    assert_eq!(at("csharp", source, 4, "x", 0), Some(kind("local", 3)));
    assert_eq!(at("csharp", source, 5, "e", 1), Some(kind("local", 5)));
    assert_eq!(at("csharp", source, 6, "z", 1), Some(kind("parameter", 6)));
    assert_eq!(at("csharp", source, 7, "n", 0), None, "a member");
}

/// C's rules, and a range for's variable, a lambda's parameter and a
/// catch's, each in its own scope.
#[test]
fn cpp_names_resolve_to_range_lambda_and_catch_variables() {
    let source = "int f(std::vector<int> &vs, int n)
{
\tfor (auto &v : vs) { n += v; }
\tauto l = [&](int y) { return y + n; };
\ttry { } catch (const Err &e) { e.what(); }
\treturn l(n);
}
";
    assert_eq!(at("cpp", source, 3, "v", 1), Some(kind("local", 3)));
    assert_eq!(at("cpp", source, 3, "n", 0), Some(kind("parameter", 1)));
    assert_eq!(at("cpp", source, 4, "y", 1), Some(kind("parameter", 4)));
    assert_eq!(at("cpp", source, 5, "e", 1), Some(kind("parameter", 5)));
    assert_eq!(at("cpp", source, 6, "l", 0), Some(kind("local", 4)));
}

/// Parameters, `let` and `var`, a loop's and a closure's variables, an
/// `if let`, and a member that isn't a local.
#[test]
fn swift_names_resolve_to_parameters_and_locals() {
    let source = "class A {
  func f(_ s: String, n: Int) -> Int {
    let x = s.count
    var y = x + n
    for i in xs { y += i }
    xs.map { q in q.w }
    if let z = opt { y += z }
    return y + self.n
  }
}
";
    assert_eq!(at("swift", source, 3, "s", 0), Some(kind("parameter", 2)));
    assert_eq!(at("swift", source, 4, "x", 0), Some(kind("local", 3)));
    assert_eq!(at("swift", source, 5, "i", 1), Some(kind("local", 5)));
    assert_eq!(at("swift", source, 6, "q", 1), Some(kind("parameter", 6)));
    assert_eq!(at("swift", source, 7, "z", 1), Some(kind("local", 7)));
    assert_eq!(at("swift", source, 8, "n", 0), None, "a member");
}

/// Parameters of each form, a block's, an assignment's and a rescue's
/// variables, and a method called that isn't a local.
#[test]
fn ruby_names_resolve_to_parameters_and_locals() {
    let source = "class A
  def f(s, n = 1, *rest, k:, &blk)
    x = s.size + n + k
    xs.each { |i| x += i }
    begin; rescue Err => e; e.go; end
    x.size + rest.size
  end
end
";
    assert_eq!(at("ruby", source, 3, "s", 0), Some(kind("parameter", 2)));
    assert_eq!(at("ruby", source, 3, "k", 0), Some(kind("parameter", 2)));
    assert_eq!(at("ruby", source, 4, "i", 1), Some(kind("parameter", 4)));
    assert_eq!(at("ruby", source, 4, "x", 0), Some(kind("local", 3)));
    assert_eq!(at("ruby", source, 5, "e", 1), Some(kind("local", 5)));
    assert_eq!(at("ruby", source, 6, "size", 0), None, "a method");
    assert_eq!(at("ruby", source, 6, "rest", 0), Some(kind("parameter", 2)));
}

/// Parameters, an assignment, `foreach`, a catch, a closure's `use`, and
/// a property that isn't a local.
#[test]
fn php_names_resolve_to_parameters_and_locals() {
    let source = "<?php
class A {
  function f(Foo $s, int $n = 1) {
    $x = $s;
    foreach ($xs as $k => $v) { $x = $v; }
    try { } catch (Err $e) { $e->go(); }
    $g = function ($y) use ($x) { return $y + $x; };
    return $this->x;
  }
}
";
    assert_eq!(at("php", source, 4, "s", 0), Some(kind("parameter", 3)));
    assert_eq!(at("php", source, 5, "v", 1), Some(kind("local", 5)));
    assert_eq!(at("php", source, 6, "e", 1), Some(kind("local", 6)));
    assert_eq!(at("php", source, 7, "y", 1), Some(kind("parameter", 7)));
    assert_eq!(at("php", source, 7, "x", 1), Some(kind("parameter", 7)));
    assert_eq!(at("php", source, 8, "x", 0), None, "a property");
}

/// Parameters, `val` and `var`, a `for`'s variable, a lambda's parameter,
/// a case's bindings and not its constant.
#[test]
fn scala_names_resolve_to_parameters_and_locals() {
    let source = "class A {
  def f(s: String, n: Int): Int = {
    val x = s.length
    var y = x + n
    for (i <- xs) { y += i }
    xs.map(q => q.w)
    xs match { case (a, Nil) => a }
    y
  }
}
";
    assert_eq!(at("scala", source, 3, "s", 0), Some(kind("parameter", 2)));
    assert_eq!(at("scala", source, 4, "x", 0), Some(kind("local", 3)));
    assert_eq!(at("scala", source, 5, "i", 1), Some(kind("local", 5)));
    assert_eq!(at("scala", source, 6, "q", 1), Some(kind("parameter", 6)));
    assert_eq!(at("scala", source, 7, "a", 1), Some(kind("local", 7)));
    assert_eq!(at("scala", source, 7, "Nil", 0), None, "a constant");
}

/// Parameters, `local`, both kinds of `for`, and a global that isn't one.
#[test]
fn lua_names_resolve_to_parameters_and_locals() {
    let source = "local function f(s, n)
  local x = s.size
  for i, v in ipairs(xs) do x = x + v end
  for k = 1, n do x = x + k end
  g = 1
  return x + g
end
";
    assert_eq!(at("lua", source, 2, "s", 0), Some(kind("parameter", 1)));
    assert_eq!(at("lua", source, 3, "v", 1), Some(kind("local", 3)));
    assert_eq!(at("lua", source, 3, "x", 1), Some(kind("local", 2)));
    assert_eq!(at("lua", source, 4, "k", 1), Some(kind("local", 4)));
    assert_eq!(at("lua", source, 6, "g", 0), None, "a global");
}

/// Parameters, `const` and `var`, payloads, and a field that isn't one.
#[test]
fn zig_names_resolve_to_parameters_payloads_and_locals() {
    let source = "fn f(s: Foo, n: u32) u32 {
    const x = s.len;
    var y: u32 = x + n;
    for (xs) |i| { y += i; }
    if (opt) |z| { y += z; }
    return y + s.n;
}
";
    assert_eq!(at("zig", source, 2, "s", 0), Some(kind("parameter", 1)));
    assert_eq!(at("zig", source, 3, "x", 0), Some(kind("local", 2)));
    assert_eq!(at("zig", source, 4, "i", 1), Some(kind("local", 4)));
    assert_eq!(at("zig", source, 5, "z", 1), Some(kind("local", 5)));
    assert_eq!(at("zig", source, 6, "n", 0), None, "a field");
}

/// A `def`'s arguments, what `=` binds, a clause's patterns, and a field
/// that isn't a local.
#[test]
fn elixir_names_resolve_to_arguments_and_bindings() {
    let source = "defmodule A do
  def f(s, m) do
    x = s + m
    Enum.map(xs, fn i -> i + x end)
    case x do
      {:ok, v} -> v
    end
    m.x
  end
end
";
    assert_eq!(at("elixir", source, 3, "s", 0), Some(kind("parameter", 2)));
    assert_eq!(at("elixir", source, 4, "x", 0), Some(kind("local", 3)));
    assert_eq!(at("elixir", source, 4, "i", 1), Some(kind("parameter", 4)));
    assert_eq!(at("elixir", source, 6, "v", 1), Some(kind("parameter", 6)));
    assert_eq!(at("elixir", source, 8, "x", 0), None, "a field");
}

/// A function's `local` and plain variables and its loop's.
#[test]
fn bash_names_resolve_to_a_functions_variables() {
    let source = "f() {
  local x=1
  for i in a b; do echo \"$i $x\"; done
  y=2
  echo \"$y\"
}
";
    assert_eq!(at("bash", source, 3, "i", 1), Some(kind("local", 3)));
    assert_eq!(at("bash", source, 3, "x", 0), Some(kind("local", 2)));
    assert_eq!(at("bash", source, 5, "y", 0), Some(kind("local", 4)));
}

/// Runs `f` on a thread with the stack a request of the navigator helper
/// has.
fn on_a_request_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .expect("spawns")
        .join()
        .expect("no overflow")
}

/// A name in a declarator or a pattern nested thousands deep is read
/// without recursing into it.
#[test]
fn deeply_nested_declarators_and_patterns_are_read() {
    let c = format!(
        "int f(void)\n{{\n\tint {}q;\n\treturn q;\n}}\n",
        "*".repeat(20_000)
    );
    let found = on_a_request_stack(move || at("c", &c, 4, "q", 0));
    assert_eq!(found, Some(kind("local", 3)));
    let js = format!(
        "function f(x) {{\n  let {}q{} = x;\n  return q;\n}}\n",
        "[".repeat(20_000),
        "]".repeat(20_000)
    );
    let found = on_a_request_stack(move || at("javascript", &js, 3, "q", 0));
    assert_eq!(found, Some(kind("local", 2)));
    // Unclosed, as a file half written or made to break it is.
    let open = format!(
        "function f(x) {{\n  let {}\n  return x;\n}}\n",
        "[".repeat(20_000)
    );
    on_a_request_stack(move || at("javascript", &open, 3, "x", 0));
}

/// A receiver thousands of nodes deep is read in time linear in its depth.
#[test]
fn a_receiver_in_deeply_nested_code_is_read() {
    let open = "(".repeat(20_000);
    let close = ")".repeat(20_000);
    let source = format!("class A {{\n  f() {{\n    return {open}this.x{close};\n  }}\n}}\n");
    let found = on_a_request_stack(move || {
        let lang = GRAMMARS.named("javascript").expect("bundled");
        let tree = parse_tree(
            lang,
            source.as_bytes(),
            std::time::Duration::from_secs(5),
            None,
        )
        .expect("parses");
        let x = source.find("this.x").expect("the use") + 5;
        let started = std::time::Instant::now();
        let found = receiver_at(lang, &tree, source.as_bytes(), x..x + 1, &|| false);
        (found.and_then(|r| r.ty), started.elapsed())
    });
    assert_eq!(found.0.as_deref(), Some("A"));
    assert!(
        found.1 < std::time::Duration::from_millis(500),
        "{:?}",
        found.1
    );
}

/// A pattern's names aren't visible in the value it's matched with, where
/// what came before them is: conditions, loops, comprehensions and
/// switches, in each language's form.
#[test]
fn a_patterns_names_are_not_visible_in_its_value() {
    let (p, l) = (|n| Some(kind("parameter", n)), |n| Some(kind("local", n)));
    let rust = "fn f(x: Option<u8>, y: T) {
    if let Some(x) = x && let Some(y) = y.get(x) {
        x;
    } else {
        x;
    }
    while let Some(x) = x.next() { x; }
    for x in x { x; }
}
";
    assert_eq!(at("rust", rust, 2, "x", 1), p(1), "if let's value");
    assert_eq!(at("rust", rust, 2, "y", 1), p(1), "a chain's");
    assert_eq!(at("rust", rust, 2, "x", 2), l(2), "past the first let");
    assert_eq!(at("rust", rust, 3, "x", 0), l(2));
    assert_eq!(at("rust", rust, 5, "x", 0), p(1), "the else");
    assert_eq!(at("rust", rust, 7, "x", 1), p(1), "while let's value");
    assert_eq!(at("rust", rust, 7, "x", 2), l(7));
    assert_eq!(at("rust", rust, 8, "x", 1), p(1), "the loop's collection");
    assert_eq!(at("rust", rust, 8, "x", 2), l(8));
    let arm = "fn f(a: A, p: P) {\n    match p {\n        (a, b) if a > b => a.g(b),\n        _ => a,\n    };\n}\n";
    assert_eq!(
        at("rust", arm, 3, "a", 2),
        l(3),
        "an arm's value is its body"
    );
    assert_eq!(at("rust", arm, 4, "a", 0), p(1));
    let go = "package p
func f(file *F, x any, c chan int) {
	for _, file := range file.Files { file() }
	switch x := x.(type) {
	case int:
		x()
	}
	for x := range x { x() }
}
";
    assert_eq!(at("go", go, 3, "file", 1), p(2));
    assert_eq!(at("go", go, 3, "file", 2), l(3));
    assert_eq!(at("go", go, 4, "x", 1), p(2));
    assert_eq!(at("go", go, 6, "x", 0), l(4));
    assert_eq!(at("go", go, 8, "x", 1), p(2));
    let python = "def f(line, x):
    [x for x in x if x]
    for line in line:
        line()
";
    assert_eq!(at("python", python, 2, "x", 0), l(2));
    assert_eq!(at("python", python, 2, "x", 2), p(1));
    assert_eq!(at("python", python, 2, "x", 3), l(2));
    assert_eq!(at("python", python, 3, "line", 1), p(1));
    assert_eq!(at("python", python, 4, "line", 0), l(3));
    let swift = "func f(_ x: X?, i: [Int]) {
  if let x = x, let y = x { x() } else { x() }
  guard let x = x else { return x }
  for i in i { i() }
  while let x = x { x() }
}
";
    assert_eq!(at("swift", swift, 2, "x", 1), p(1));
    assert_eq!(at("swift", swift, 2, "x", 2), l(2), "past the first let");
    assert_eq!(at("swift", swift, 2, "x", 3), l(2));
    assert_eq!(at("swift", swift, 2, "x", 4), p(1), "the else");
    assert_eq!(at("swift", swift, 3, "x", 1), p(1));
    assert_eq!(at("swift", swift, 3, "x", 2), p(1), "the guard's else");
    assert_eq!(at("swift", swift, 4, "i", 1), p(1));
    assert_eq!(at("swift", swift, 4, "i", 2), l(4));
    assert_eq!(at("swift", swift, 5, "x", 2), l(5));
    let kotlin = "fun f(x: X) {\n  for (x in x) { x() }\n}\n";
    assert_eq!(at("kotlin", kotlin, 2, "x", 1), p(1));
    assert_eq!(at("kotlin", kotlin, 2, "x", 2), l(2));
    let scala = "class A {\n  def f(x: X) = {\n    for (x <- x) { x.g() }\n  }\n}\n";
    assert_eq!(at("scala", scala, 3, "x", 1), p(2));
    assert_eq!(at("scala", scala, 3, "x", 2), l(3));
    let lua = "local function f(x, n)\n  for i, x in pairs(x) do x() end\n  for n = 1, n do n() end\nend\n";
    assert_eq!(at("lua", lua, 2, "x", 1), p(1));
    assert_eq!(at("lua", lua, 2, "x", 2), l(2));
    assert_eq!(at("lua", lua, 3, "n", 1), p(1));
    assert_eq!(at("lua", lua, 3, "n", 2), l(3));
    let csharp = "class A {\n  void F(X x) {\n    foreach (var x in x) { x(); }\n  }\n}\n";
    assert_eq!(at("csharp", csharp, 3, "x", 1), p(2));
    assert_eq!(at("csharp", csharp, 3, "x", 2), l(3));
    let java = "class A {\n  void f(X x) {\n    for (T x : x) { x.g(); }\n  }\n}\n";
    assert_eq!(at("java", java, 3, "x", 1), p(2));
    assert_eq!(at("java", java, 3, "x", 2), l(3));
    let cpp = "int f(X x)\n{\n\tfor (auto &x : x) { x(); }\n}\n";
    assert_eq!(at("cpp", cpp, 3, "x", 1), p(1));
    assert_eq!(at("cpp", cpp, 3, "x", 2), l(3));
}

/// In C and C++ a name is in scope right past its declarator, its own
/// initializer included.
#[test]
fn c_names_are_visible_in_their_own_initializers() {
    let source = "struct foo { int x; };
int b;
void f(int item, int cnt)
{
	{
		struct foo *item = kzalloc(sizeof(*item), GFP_KERNEL);
		int cnt = 1, b = cnt;
	}
}
static struct foo list = { .x = sizeof(list) };
";
    for language in ["c", "cpp"] {
        assert_eq!(at(language, source, 6, "item", 1), Some(kind("local", 6)));
        assert_eq!(at(language, source, 7, "cnt", 1), Some(kind("local", 7)));
        assert_eq!(
            at(language, source, 10, "list", 1),
            Some(kind("variable", 10))
        );
    }
}

/// Python's `global` names the module's, and `nonlocal` the function's
/// around; a class's body in a function is no scope its methods see.
#[test]
fn python_names_follow_global_nonlocal_and_class_bodies() {
    let source = "def f(n, y):
    def g():
        global total
        nonlocal n
        total = 1
        n = 2
        return total + n
    class A:
        y = 1
        z = y
        def m(self):
            return y
    return y
";
    assert_eq!(at("python", source, 7, "total", 0), None, "the module's");
    assert_eq!(
        at("python", source, 5, "total", 0),
        None,
        "the module's, where it's set"
    );
    assert_eq!(at("python", source, 7, "n", 0), Some(kind("parameter", 1)));
    assert_eq!(at("python", source, 10, "y", 0), None, "the class's own");
    assert_eq!(at("python", source, 12, "y", 0), Some(kind("parameter", 1)));
    assert_eq!(at("python", source, 13, "y", 0), Some(kind("parameter", 1)));
}

/// JavaScript's `var` is its function's, whatever block it's in; `let` and
/// `const` are their block's.
#[test]
fn javascript_var_is_the_functions() {
    let source = "function f(b, c) {
  if (b) { var a = 1; let c = 2; }
  for (var i = 0; i < 3; i++) {}
  return a + c + i;
}
";
    for language in ["javascript", "typescript"] {
        assert_eq!(at(language, source, 4, "a", 0), Some(kind("local", 2)));
        assert_eq!(at(language, source, 4, "c", 0), Some(kind("parameter", 1)));
        assert_eq!(at(language, source, 4, "i", 0), Some(kind("local", 3)));
    }
}

/// A named argument's label is the callee's parameter, not a local.
#[test]
fn named_argument_labels_are_no_locals() {
    let kotlin = "fun f(name: Int) {\n  g(name = name)\n}\n";
    assert_eq!(at("kotlin", kotlin, 2, "name", 0), None);
    assert_eq!(
        at("kotlin", kotlin, 2, "name", 1),
        Some(kind("parameter", 1))
    );
    let csharp = "class A {\n  void F(int name) {\n    G(name: name);\n  }\n}\n";
    assert_eq!(at("csharp", csharp, 3, "name", 0), None);
    assert_eq!(
        at("csharp", csharp, 3, "name", 1),
        Some(kind("parameter", 2))
    );
}

/// What a minifier makes of a program: thousands of functions, in one, on
/// one line, each name reused.
fn minified(functions: usize) -> String {
    let mut out = String::from("(function(e,t){");
    for i in 0..functions {
        out.push_str("function a");
        out.push_str(&i.to_string());
        out.push_str("(e,t){var n=e+t;return function(r){return r+n+e}}");
    }
    out.push_str("return e})();");
    out
}

/// A resolution asks whether to stop throughout: told to, it finds nothing,
/// soon.
#[test]
fn a_resolution_stops_when_told_to() {
    let source = minified(2_000);
    let lang = GRAMMARS.named("javascript").expect("bundled");
    let tree = parse_tree(
        lang,
        source.as_bytes(),
        std::time::Duration::from_secs(30),
        None,
    )
    .expect("parses");
    let e = source.rfind("return e})").expect("the use") + 7;
    let resolve =
        |stop: &dyn Fn() -> bool| local_at(lang, &tree, source.as_bytes(), e..e + 1, stop);
    let asked = std::cell::Cell::new(0);
    let found = resolve(&|| {
        asked.set(asked.get() + 1);
        false
    });
    assert_eq!(found.map(|l| l.kind.name()), Some("parameter"));
    let all = asked.get();
    assert!(all > 20, "asked {all} times");
    asked.set(0);
    let stopped = resolve(&|| {
        asked.set(asked.get() + 1);
        asked.get() > 5
    });
    assert_eq!(stopped, None);
    assert!(asked.get() < 10, "went on for {} asks", asked.get());
}

/// Every definition of a name reused thousands of times is weighed against
/// the scopes around it once: no slower by the square of their number.
#[test]
fn a_reused_name_is_resolved_in_one_sweep() {
    let took = |functions| {
        let source = minified(functions);
        let lang = GRAMMARS.named("javascript").expect("bundled");
        let tree = parse_tree(
            lang,
            source.as_bytes(),
            std::time::Duration::from_secs(30),
            None,
        )
        .expect("parses");
        let e = source.rfind("return e})").expect("the use") + 7;
        (0..3)
            .map(|_| {
                let started = std::time::Instant::now();
                local_at(lang, &tree, source.as_bytes(), e..e + 1, &|| false).expect("a parameter");
                started.elapsed()
            })
            .min()
            .expect("runs")
    };
    // Eight times the names: 64 times the work, were it quadratic.
    let (small, large) = (took(500), took(4_000));
    assert!(large < small * 24, "{small:?}, then {large:?}");
}
