use proptest::prelude::*;

use super::*;

/// Line pool with repeats, whitespace variants, CRLF, and a final line
/// without a newline, so equal-under-normalization lines are common.
const POOL: &[&str] = &[
    "a\n", "b\n", "c\n", "  a\n", "a  \n", "a\r\n", "\tb\n", "b \t\n", "{\n", "}\n", "\n", "x y\n",
    "x  y\n", "z",
];

fn text(ids: &[usize]) -> Text {
    let mut s = String::new();
    for (k, &i) in ids.iter().enumerate() {
        s.push_str(POOL[i]);
        // The newline-less pool line only stays newline-less at the end.
        if !POOL[i].ends_with('\n') && k + 1 != ids.len() {
            s.push('\n');
        }
    }
    Text::new(s.into_bytes())
}

fn myers_changes(old: &Text, new: &Text, ws: Whitespace) -> Changes {
    diff_with(old, new, ws, |a, b, _, r, d| myers::diff(a, b, r, d))
}

fn tiny_cutoff_changes(old: &Text, new: &Text, limit: usize) -> Changes {
    diff_with(old, new, Whitespace::Exact, |a, b, _, r, d| {
        myers::diff_with_limit(a, b, r, d, limit);
    })
}

fn norm(line: &[u8], ws: Whitespace) -> Vec<u8> {
    let t = Text::new(line.to_vec());
    let mut arena = Vec::new();
    normalize(&t, ws, &mut arena);
    arena
}

/// Unflagged lines must pair up in order and be equal under `ws`.
fn check_changes(old: &Text, new: &Text, c: &Changes, ws: Whitespace) {
    let kept_old: Vec<_> = (0..old.len()).filter(|&i| !c.removed[i]).collect();
    let kept_new: Vec<_> = (0..new.len()).filter(|&j| !c.added[j]).collect();
    assert_eq!(
        kept_old.len(),
        kept_new.len(),
        "unchanged lines must pair up"
    );
    for (&i, &j) in kept_old.iter().zip(&kept_new) {
        assert_eq!(
            norm(old.line(i), ws),
            norm(new.line(j), ws),
            "line {i} vs {j}"
        );
    }
}

/// Rebuilds the new text from the old one plus hunks. Context lines are taken
/// from the new side, so under -w/-b only the hunk regions are exact.
fn apply(old: &Text, new: &Text, hunks: &[Hunk]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut o = 0;
    let mut last_end = 0;
    for h in hunks {
        assert!(
            h.old_start >= last_end,
            "hunks must be ordered and disjoint"
        );
        while o < h.old_start {
            out.extend_from_slice(old.line(o));
            o += 1;
        }
        let (mut old_seen, mut new_seen) = (0, 0);
        for line in &h.lines {
            match line.kind {
                LineKind::Context => {
                    assert_eq!(line.old, o);
                    out.extend_from_slice(new.line(line.new));
                    o += 1;
                    old_seen += 1;
                    new_seen += 1;
                }
                LineKind::Removed => {
                    assert_eq!(line.old, o);
                    o += 1;
                    old_seen += 1;
                }
                LineKind::Added => {
                    out.extend_from_slice(new.line(line.new));
                    new_seen += 1;
                }
            }
        }
        assert_eq!(
            (old_seen, new_seen),
            (h.old_len, h.new_len),
            "header lengths"
        );
        last_end = h.old_start + h.old_len;
    }
    while o < old.len() {
        out.extend_from_slice(old.line(o));
        o += 1;
    }
    out
}

fn ids(max_len: usize) -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(0..POOL.len(), 0..max_len)
}

/// Mostly two lines, with a few rare ones: the rare lines seed a split, and
/// the halves still hold lines too frequent for histogram, which sends them
/// to the Myers fallback.
fn frequent() -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(prop_oneof![100 => 0..2usize, 1 => 2..POOL.len()], 150..600)
}

/// An old/new pair where new is old with random edits, so the two share
/// structure the way real revisions do.
fn edited(
    old: impl Strategy<Value = Vec<usize>>,
) -> impl Strategy<Value = (Vec<usize>, Vec<usize>)> {
    (old, prop::collection::vec((0..4u8, 0..POOL.len()), 0..20)).prop_map(|(old, edits)| {
        let mut new = old.clone();
        for (k, (op, v)) in edits.into_iter().enumerate() {
            let at = if new.is_empty() {
                0
            } else {
                (k * 7919 + v) % new.len()
            };
            match op {
                0 => new.insert(at, v),
                1 if !new.is_empty() => {
                    new.remove(at);
                }
                2 if !new.is_empty() => new[at] = v,
                _ => new.push(v),
            }
        }
        (old, new)
    })
}

fn pair() -> impl Strategy<Value = (Vec<usize>, Vec<usize>)> {
    prop_oneof![
        edited(ids(80)),
        (ids(40), ids(40)),
        edited(ids(400)),
        edited(frequent()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn histogram_hunks_reproduce_new((o, n) in pair(), context in 0usize..5) {
        let (old, new) = (text(&o), text(&n));
        let c = changes(&old, &new, Whitespace::Exact);
        check_changes(&old, &new, &c, Whitespace::Exact);
        let hunks = build_hunks(&old, &c, context);
        prop_assert_eq!(apply(&old, &new, &hunks), new.bytes());
    }

    #[test]
    fn myers_hunks_reproduce_new((o, n) in pair(), context in 0usize..5) {
        let (old, new) = (text(&o), text(&n));
        let c = myers_changes(&old, &new, Whitespace::Exact);
        check_changes(&old, &new, &c, Whitespace::Exact);
        let hunks = build_hunks(&old, &c, context);
        prop_assert_eq!(apply(&old, &new, &hunks), new.bytes());
    }

    #[test]
    fn myers_cost_cutoff_stays_valid((o, n) in pair(), limit in 1usize..4) {
        let (old, new) = (text(&o), text(&n));
        let c = tiny_cutoff_changes(&old, &new, limit);
        check_changes(&old, &new, &c, Whitespace::Exact);
        prop_assert_eq!(apply(&old, &new, &build_hunks(&old, &c, 3)), new.bytes());
    }

    #[test]
    fn whitespace_modes_are_valid(
        (o, n) in pair(),
        ws in prop_oneof![Just(Whitespace::IgnoreAll), Just(Whitespace::IgnoreChange)],
        use_myers in any::<bool>(),
    ) {
        let (old, new) = (text(&o), text(&n));
        let c = if use_myers { myers_changes(&old, &new, ws) } else { changes(&old, &new, ws) };
        check_changes(&old, &new, &c, ws);
        let applied = Text::new(apply(&old, &new, &build_hunks(&old, &c, 3)));
        let applied: Vec<_> = applied.lines().map(|l| norm(l, ws)).collect();
        let expected: Vec<_> = new.lines().map(|l| norm(l, ws)).collect();
        prop_assert_eq!(applied, expected);
    }

    #[test]
    fn emphasis_stays_on_token_boundaries((o, n) in edited(ids(60))) {
        let (old, new) = (text(&o), text(&n));
        for h in hunks(&old, &new, &Options::default()) {
            for line in &h.lines {
                let bytes = match line.kind {
                    LineKind::Removed => old.line(line.old),
                    LineKind::Added => new.line(line.new),
                    LineKind::Context => {
                        prop_assert!(line.emphasis.is_empty());
                        continue;
                    }
                };
                let bounds: Vec<usize> = tokenize(bytes).iter().map(|r| r.start).collect();
                for r in &line.emphasis {
                    prop_assert!(r.start < r.end && r.end <= bytes.len());
                    prop_assert!(bounds.contains(&r.start));
                }
            }
        }
    }

    #[test]
    fn tokens_cover_the_line(line in prop::collection::vec(any::<u8>(), 0..200)) {
        let tokens = tokenize(&line);
        let mut at = 0;
        for t in &tokens {
            prop_assert_eq!(t.start, at);
            prop_assert!(t.end > t.start);
            at = t.end;
        }
        prop_assert_eq!(at, line.len());
    }
}

#[test]
fn frequent_lines_fall_back_to_myers() {
    // Every common line occurs more than MAX_CHAIN_LEN times, so histogram
    // can't seed a region and must hand the range to Myers.
    let old = Text::new("a\nb\n".repeat(100).into_bytes());
    let new = Text::new("b\na\n".repeat(100).into_bytes());
    let c = changes(&old, &new, Whitespace::Exact);
    check_changes(&old, &new, &c, Whitespace::Exact);
    assert_eq!(c, myers_changes(&old, &new, Whitespace::Exact));
}

#[test]
fn hunk_headers_and_merging() {
    let line = |i: usize| match i {
        3 => "three".to_owned(),
        9 => "nine".to_owned(),
        i => i.to_string(),
    };
    let old: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    let new: Vec<String> = (0..19).map(line).collect();
    let text = |lines: Vec<String>| Text::new((lines.join("\n") + "\n").into_bytes());
    let (old, new) = (text(old), text(new));
    let h = hunks(&old, &new, &Options::default());
    // 3 and 9 are 5 lines apart (<= 2 * 3) so they share a hunk; 19 doesn't.
    assert_eq!(h.len(), 2);
    assert_eq!(
        (h[0].old_start, h[0].old_len, h[0].new_start, h[0].new_len),
        (0, 13, 0, 13)
    );
    assert_eq!(
        (h[1].old_start, h[1].old_len, h[1].new_start, h[1].new_len),
        (16, 4, 16, 3)
    );
}

#[test]
fn missing_final_newline_is_a_change() {
    let old = Text::new(b"a\nb\n".to_vec());
    let new = Text::new(b"a\nb".to_vec());
    let h = hunks(&old, &new, &Options::default());
    assert_eq!(h.len(), 1);
    let kinds: Vec<_> = h[0].lines.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        [LineKind::Context, LineKind::Removed, LineKind::Added]
    );
}

#[test]
fn similar_lines_get_token_emphasis() {
    let old = Text::new(b"let total = count + 1;\nunrelated old line here\n".to_vec());
    let new = Text::new(b"let total = count + 2;\nsomething else entirely now\n".to_vec());
    let h = hunks(&old, &new, &Options::default());
    let lines = &h[0].lines;
    assert_eq!(lines[0].emphasis, vec![20..21]);
    assert_eq!(lines[2].emphasis, vec![20..21]);
    assert!(lines[1].emphasis.is_empty() && lines[3].emphasis.is_empty());
}

#[test]
fn whitespace_normalization() {
    assert_eq!(norm(b" a \t b  \n", Whitespace::IgnoreChange), b" a b");
    assert_eq!(norm(b" a \t b  \n", Whitespace::IgnoreAll), b"ab");
}

/// A line and its edit are paired, even when the edit is a line ending
/// alone and no token differs; a rewrite with nothing in common isn't, and
/// neither is a line wholly added. `pair` agrees with `hunks`.
#[test]
fn edited_lines_are_paired_and_rewrites_are_not() {
    let old = Text::new(b"keep\nint a = 1;\nsame\r\nfoo bar baz\nkeep\n".to_vec());
    let new = Text::new(b"keep\nlong a = 1;\nsame\nxyz qq\nadded\nkeep\n".to_vec());
    let mut hunks = hunks_without_emphasis(&old, &new, &Options::default());
    for hunk in &mut hunks {
        super::pair(&old, &new, hunk);
    }
    let paired: Vec<(LineKind, bool)> = hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| (l.kind, l.paired))
        .collect();
    assert_eq!(
        paired,
        [
            (LineKind::Removed, true),
            (LineKind::Removed, true),
            (LineKind::Removed, false),
            (LineKind::Added, true),
            (LineKind::Added, true),
            (LineKind::Added, false),
            (LineKind::Added, false),
        ]
    );
    let crlf = &hunks[0].lines[2];
    assert!(crlf.paired && emphasize(&old, &new, &hunks[0])[2].is_empty());
    assert_eq!(
        hunks,
        super::hunks(&old, &new, &Options::default())
            .into_iter()
            .map(|mut h| {
                for l in &mut h.lines {
                    l.emphasis.clear();
                }
                h
            })
            .collect::<Vec<_>>()
    );
}

/// A line that starts a block keeps the removed lines before it from
/// pairing with the added lines after it, however alike they are.
#[test]
fn pairing_stops_at_a_block_start() {
    let old = Text::new(b"one two three\n".to_vec());
    let new = Text::new(b"one two four\n".to_vec());
    let mut hunk = hunks(&old, &new, &Options::default()).remove(0);
    assert!(hunk.lines.iter().all(|l| l.paired));
    hunk.lines[1].block_start = true;
    super::pair(&old, &new, &mut hunk);
    assert!(hunk.lines.iter().all(|l| !l.paired));
}

/// Units change whole: matched units stay unchanged, and a line inside a
/// changed unit (`shared`) changes with it even though it has an equal on
/// the other side.
#[test]
fn unit_hunks_change_whole_units() {
    let old = Text::new(b"# Title\nsame\npara one\nshared\nend\n".to_vec());
    let new = Text::new(b"# Title\nsame\npara two\nshared\nend\nx\n".to_vec());
    let hunks = unit_hunks(
        &old,
        &new,
        [&[(1, 2), (2, 2), (3, 1)], &[(1, 2), (4, 2), (3, 1), (5, 1)]],
        0,
    );
    let kinds: Vec<(LineKind, usize, usize)> = hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| (l.kind, l.old, l.new))
        .collect();
    assert_eq!(
        kinds,
        [
            (LineKind::Removed, 2, 2),
            (LineKind::Removed, 3, 2),
            (LineKind::Added, 4, 2),
            (LineKind::Added, 4, 3),
            (LineKind::Added, 5, 5),
        ]
    );
}
