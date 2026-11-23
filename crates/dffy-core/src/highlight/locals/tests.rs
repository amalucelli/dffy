use std::sync::LazyLock;

use crate::highlight::{Registry, local_at, parse_tree};

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
            "c",
            "cpp",
            "go",
            "javascript",
            "python",
            "rust",
            "tsx",
            "typescript"
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
