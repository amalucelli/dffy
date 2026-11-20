use std::fmt::Write as _;

use super::*;

fn text(s: &str) -> Text {
    Text::new(s.as_bytes().to_vec())
}

fn resolved(s: &str, choices: &[Option<Choice>]) -> String {
    let t = text(s);
    let regions = regions(&t, &Sides::default());
    String::from_utf8(resolve(&t, &regions, choices).text.bytes().to_vec()).expect("utf-8")
}

const TWO_WAY: &str = "\
top
<<<<<<< HEAD
ours 1
ours 2
=======
theirs
>>>>>>> feature
bottom
";

const DIFF3: &str = "\
<<<<<<< HEAD
ours
||||||| base
base
=======
theirs
>>>>>>> feature
";

#[test]
fn markers_are_seven_characters_then_a_label_or_nothing() {
    assert_eq!(marker(b"<<<<<<< HEAD\n"), Some(Marker::Start));
    assert_eq!(
        marker(b"||||||| merged common ancestors\n"),
        Some(Marker::Base)
    );
    assert_eq!(marker(b"=======\n"), Some(Marker::Separator));
    assert_eq!(marker(b">>>>>>>\r\n"), Some(Marker::End));
    assert_eq!(marker(b">>>>>>> feature"), Some(Marker::End));
    for text in [
        &b"======= not a marker\n"[..],
        b"<<<<<<<< eight\n",
        b"<<<<<<<HEAD\n",
        b"x <<<<<<< y\n",
        b"<<<<<<\n",
        b"\n",
    ] {
        assert_eq!(marker(text), None, "{}", String::from_utf8_lossy(text));
    }
}

#[test]
fn a_two_way_conflict_resolves_to_each_side_or_both() {
    let t = text(TWO_WAY);
    let found = regions(&t, &Sides::default());
    assert_eq!(
        found,
        vec![Region {
            start: 1,
            ours: 2..4,
            base: None,
            theirs: 5..6,
            end: 6,
        }]
    );
    assert_eq!(
        resolved(TWO_WAY, &[Some(Choice::Ours)]),
        "top\nours 1\nours 2\nbottom\n"
    );
    assert_eq!(
        resolved(TWO_WAY, &[Some(Choice::Theirs)]),
        "top\ntheirs\nbottom\n"
    );
    assert_eq!(
        resolved(TWO_WAY, &[Some(Choice::Both)]),
        "top\nours 1\nours 2\ntheirs\nbottom\n"
    );
    // Without a base, choosing it leaves the region as it is.
    assert_eq!(resolved(TWO_WAY, &[Some(Choice::Base)]), TWO_WAY);
    assert_eq!(resolved(TWO_WAY, &[None]), TWO_WAY);
    let r = resolve(&t, &found, &[Some(Choice::Both)]);
    assert_eq!(r.at, vec![1..4]);
}

#[test]
fn a_diff3_conflict_has_its_base_to_choose() {
    let t = text(DIFF3);
    let found = regions(&t, &Sides::default());
    assert_eq!(found[0].base, Some(3..4));
    assert_eq!(found[0].ours, 1..2);
    assert_eq!(found[0].theirs, 5..6);
    assert_eq!(resolved(DIFF3, &[Some(Choice::Base)]), "base\n");
    assert_eq!(resolved(DIFF3, &[Some(Choice::Both)]), "ours\ntheirs\n");
}

/// Marker-looking lines that aren't markers stay text, as do markers
/// outside a region and a `<<<<<<<` nothing closes.
#[test]
fn lines_that_only_look_like_markers_are_text() {
    let s = "\
======= not a marker
=======
>>>>>>> stray
<<<<<<<< eight
<<<<<<< HEAD
a <<<<<<< b
=======
c
>>>>>>> feature
<<<<<<< never closed
tail
";
    let found = regions(&text(s), &Sides::default());
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].start, found[0].end), (4, 8));
    assert_eq!(
        resolved(s, &[Some(Choice::Theirs)]),
        "======= not a marker\n=======\n>>>>>>> stray\n<<<<<<<< eight\nc\n<<<<<<< never closed\ntail\n"
    );
}

/// A side whose own code has a `=======` line reads two ways; the index's
/// sides tell which, and without them the first `=======` wins.
#[test]
fn the_index_sides_settle_a_separator_inside_a_side() {
    let s = "\
<<<<<<< HEAD
Title
=======
ours
=======
theirs
>>>>>>> feature
";
    let t = text(s);
    let first = regions(&t, &Sides::default());
    assert_eq!(first[0].ours, 1..2, "the first reading");
    let ours = text("Title\n=======\nours\n");
    let theirs = text("theirs\n");
    let sides = Sides {
        ours: Some(&ours),
        theirs: Some(&theirs),
        base: None,
    };
    let settled = regions(&t, &sides);
    assert_eq!(settled[0].ours, 1..4);
    assert_eq!(settled[0].theirs, 5..6);
}

#[test]
fn crlf_lines_keep_their_endings() {
    let s = "a\r\n<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> x\r\nb\r\n";
    assert_eq!(resolved(s, &[Some(Choice::Theirs)]), "a\r\ntheirs\r\nb\r\n");
}

/// git ends a side without a final newline with one before the next
/// marker; the file's last line gets its missing newline back.
#[test]
fn a_conflict_ending_the_file_without_a_newline_resolves_without_one() {
    let s = "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> x";
    assert_eq!(resolved(s, &[Some(Choice::Ours)]), "ours");
    assert_eq!(resolved(s, &[Some(Choice::Both)]), "ours\ntheirs");
    let crlf = "<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> x";
    assert_eq!(resolved(crlf, &[Some(Choice::Theirs)]), "theirs");
    assert_eq!(resolved(&format!("{s}\n"), &[Some(Choice::Ours)]), "ours\n");
}

/// The preview diff removes a chosen region and adds what it keeps; one
/// without a choice shows unchanged, in a hunk of its own or one shared
/// with a region whose context it meets.
#[test]
fn the_preview_shows_every_region_chosen_or_not() {
    let numbered = |word: &str| {
        (0..10).fold(String::new(), |mut out, i| {
            let _ = writeln!(out, "{word} {i}");
            out
        })
    };
    let s = [
        numbered("line").as_str(),
        TWO_WAY
            .trim_start_matches("top\n")
            .trim_end_matches("bottom\n"),
        "between\n",
        DIFF3,
        numbered("after").as_str(),
    ]
    .concat();
    let t = text(&s);
    let found = regions(&t, &Sides::default());
    assert_eq!(found.len(), 2);
    let choices = [Some(Choice::Theirs), None];
    let r = resolve(&t, &found, &choices);
    let hunks = preview_hunks(&t, &r, &found, &choices, 3);
    assert_eq!(hunks.len(), 1, "the regions' contexts meet");
    let h = &hunks[0];
    assert_eq!((h.old_start, h.new_start), (7, 7));
    let kinds: String = h
        .lines
        .iter()
        .map(|l| match l.kind {
            LineKind::Context => ' ',
            LineKind::Removed => '-',
            LineKind::Added => '+',
        })
        .collect();
    assert_eq!(kinds, "   ------+ ".to_owned() + "       " + "   ");
    assert_eq!(
        h.old_len,
        h.lines.iter().filter(|l| l.kind != LineKind::Added).count()
    );
    // Every line maps to its own line on each side.
    for l in &h.lines {
        match l.kind {
            LineKind::Context => assert_eq!(bare(t.line(l.old)), bare(r.text.line(l.new))),
            LineKind::Removed => assert!(l.old < t.len()),
            LineKind::Added => assert_eq!(r.text.line(l.new), b"theirs\n"),
        }
    }
    assert_eq!(h.new_len, r.text.len() - 7 - 7);
    // Far apart, each region has its own hunk.
    let spread = s.replace("between\n", &"between\n".repeat(10));
    let t = text(&spread);
    let found = regions(&t, &Sides::default());
    let r = resolve(&t, &found, &[None, None]);
    let hunks = preview_hunks(&t, &r, &found, &[None, None], 3);
    assert_eq!(hunks.len(), 2);
    assert!(
        hunks
            .iter()
            .flat_map(|h| &h.lines)
            .all(|l| l.kind == LineKind::Context)
    );
}
