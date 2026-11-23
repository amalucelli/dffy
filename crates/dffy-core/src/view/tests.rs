use proptest::prelude::*;

use super::*;

proptest! {
    /// Read on one side, a hunk's rows show each line of that side once,
    /// in order, a no-newline marker with the line it follows. A changed
    /// line pairs with the hidden line split view pairs it with, and the
    /// hidden lines split view leaves unpaired, and only those, stand in
    /// rows of their own: every hidden line is on exactly one row.
    #[test]
    fn side_rows_show_one_side_and_pair_the_other_as_split_does(
        picks in prop::collection::vec((0..3u8, any::<bool>()), 0..40),
        (old_marker, marker) in any::<(bool, bool)>(),
        old in any::<bool>(),
    ) {
        let mut lines: Vec<(Kind, bool)> = (picks.into_iter())
            .map(|(p, starts)| match p {
                0 => (Kind::Context, false),
                1 => (Kind::Removed, starts),
                _ => (Kind::Added, starts),
            })
            .collect();
        // A marker follows a side's last line: the old side's last removed
        // line, with only added lines after it, and the hunk's last line.
        if let Some(r) = lines.iter().rposition(|(k, _)| *k == Kind::Removed)
            && old_marker
            && lines[r + 1..].iter().all(|(k, _)| *k == Kind::Added)
        {
            lines.insert(r + 1, (Kind::NoNewline, false));
        }
        if marker && lines.last().is_some_and(|(k, _)| *k != Kind::NoNewline) {
            lines.push((Kind::NoNewline, false));
        }
        let side = if old { Side::Old } else { Side::New };
        let hidden = if old { Kind::Added } else { Kind::Removed };
        let mut rows = Vec::new();
        side_rows(side, lines.len(), |i| lines[i], &mut rows);
        let mut split = Vec::new();
        super::rows(Arrangement::Split, lines.len(), |i| lines[i], &mut split);

        let kept = |i: usize| match lines[i].0 {
            Kind::NoNewline => lines[i - 1].0 != hidden,
            k => k != hidden,
        };
        let shown: Vec<usize> = (0..lines.len()).filter(|&i| kept(i)).collect();
        let one: Vec<usize> = (rows.iter())
            .filter_map(|r| match *r {
                Row::One { line, .. } => Some(line),
                _ => None,
            })
            .collect();
        prop_assert_eq!(one, shown);
        // Split view's pairs, and its hidden side's lines left unpaired.
        let (mut pairs, mut alone) = (Vec::new(), Vec::new());
        for row in &split {
            if let Row::Pair(r, a) = *row
                && r != a
            {
                let (this, other) = if old { (r, a) } else { (a, r) };
                match (this, other) {
                    (Some(t), Some(o)) => pairs.push((t, o)),
                    (None, Some(o)) => alone.push(o),
                    _ => {}
                }
            }
        }
        let paired: Vec<(usize, usize)> = (rows.iter())
            .filter_map(|r| match *r {
                Row::One { line, paired } => paired.map(|p| (line, p)),
                _ => None,
            })
            .collect();
        prop_assert_eq!(paired, pairs);
        let ruled: Vec<usize> = (rows.iter())
            .flat_map(|r| match *r {
                Row::Hidden { from, n } => from..from + n as usize,
                _ => 0..0,
            })
            .collect();
        prop_assert_eq!(ruled, alone);
        prop_assert_eq!(rows.is_empty(), lines.is_empty());
    }
}

#[test]
fn heading_is_the_nearest_unindented_line_above() {
    let text = Text::new(b"int a;\nstatic void f(void)\n{\n\tx();\n\ty();\n}\n".to_vec());
    let mut h = Heading::default();
    assert_eq!(h.find(&text, 0), b"");
    assert_eq!(h.find(&text, 3), b"static void f(void)");
    assert_eq!(h.find(&text, 5), b"static void f(void)");
}

/// Trailing whitespace is Unicode's, vertical tabs and no-break spaces
/// included; a line ending in invalid UTF-8 keeps it.
#[test]
fn heading_text_drops_trailing_unicode_whitespace() {
    let text = Text::new(b"a \t\x0b\r\nb\xc2\xa0\xe3\x80\x80\nc \xff\nd\xe2\x80\n".to_vec());
    let headings: Vec<&[u8]> = (0..text.len()).map(|i| heading_text(&text, i)).collect();
    assert_eq!(headings, [&b"a"[..], b"b", b"c \xff", b"d\xe2\x80"]);
}
