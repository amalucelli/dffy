use super::*;

#[test]
fn strips_sgr_and_osc_sequences() {
    let mut out = Vec::new();
    strip_ansi(
        b"\x1b[1mdiff --git\x1b[m a\x1b]8;;http://x\x07link\x1b]8;;\x1b\\",
        &mut out,
    );
    assert_eq!(out, b"diff --git alink");
    strip_ansi(b"\x1b[32m+\x1b[m\x1b[32madded\x1b[m", &mut out);
    assert_eq!(out, b"+added");
    strip_ansi(b"trailing\x1b", &mut out);
    assert_eq!(out, b"trailing");
}

#[test]
fn hunk_headers_default_lengths_to_one() {
    let h = parse_hunk_header(b"@@ -3 +4,0 @@ fn main()").expect("valid");
    assert_eq!(
        (h.old_start, h.old_len, h.new_start, h.new_len),
        (3, 1, 4, 0)
    );
    assert_eq!(h.heading, b"fn main()");
    assert!(parse_hunk_header(b"@@ -x +1 @@").is_none());
    assert!(parse_hunk_header(b"@@@ -1,2 -1,2 +1,3 @@@").is_none());
    // Ends past the largest line number.
    assert!(parse_hunk_header(b"@@ -18446744073709551615 +1 @@").is_none());
    assert!(parse_hunk_header(b"@@ -5 +5,18446744073709551615 @@").is_none());
}

#[test]
fn modes_are_octal_digits_only() {
    let mut parser = Parser::default();
    let mut modes = None;
    for line in [
        &b"diff --git a/x b/x"[..],
        b"old mode 100644\x07\xc2\x9b2J",
        b"new mode 100755",
    ] {
        parser
            .feed(line, &mut |e| {
                if let Event::File(f) = e {
                    modes = Some((f.old_mode.clone(), f.new_mode.clone()));
                }
                Ok::<(), ()>(())
            })
            .expect("feeds");
    }
    parser
        .finish(&mut |e| {
            if let Event::File(f) = e {
                modes = Some((f.old_mode.clone(), f.new_mode.clone()));
            }
            Ok::<(), ()>(())
        })
        .expect("finishes");
    assert_eq!(modes, Some((None, Some("100755".to_owned()))));
}

/// The events of `lines`, as text: `file <old> <new>`, `hunk <heading>`,
/// `+`/`-`/` ` lines, `end`, `stray <line>`, `text <line>`.
fn events(lines: &[&[u8]]) -> Vec<String> {
    let mut parser = Parser::default();
    let mut out = Vec::new();
    let mut take = |e: Event| {
        let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        out.push(match e {
            Event::File(f) => format!("file {} {}", s(&f.old_path), s(&f.new_path)),
            Event::Hunk(h) => format!("hunk {}", s(h.heading)),
            Event::Line(Kind::Added, t) => format!("+{}", s(t)),
            Event::Line(Kind::Removed, t) => format!("-{}", s(t)),
            Event::Line(_, t) => format!(" {}", s(t)),
            Event::HunkEnd => "end".to_owned(),
            Event::Stray(t) => format!("stray {}", s(t)),
            Event::Text(_, t) | Event::Combined { line: t, .. } => {
                format!("text {}", s(t))
            }
        });
        Ok::<(), ()>(())
    };
    for line in lines {
        parser.feed(line, &mut take).expect("feeds");
    }
    parser.finish(&mut take).expect("finishes");
    out
}

/// A line past a hunk's counts ends the hunk and shows as no change;
/// the file's next hunk is still read as one.
#[test]
fn a_stray_line_past_the_counts_resyncs_at_the_next_hunk() {
    let got = events(&[
        b"diff --git a/x b/x",
        b"--- a/x",
        b"+++ b/x",
        b"@@ -1 +1 @@",
        b"-a",
        b"+b",
        b"+stray",
        b"@@ -5 +5 @@ f",
        b"-c",
        b"+d",
    ]);
    assert_eq!(
        got,
        [
            "file x x",
            "hunk ",
            "-a",
            "+b",
            "end",
            "stray +stray",
            "hunk f",
            "-c",
            "+d",
            "end",
        ]
    );
}

/// A patch saved with CRLF line ends: headers lose the carriage return,
/// hunk lines keep it as content.
#[test]
fn crlf_headers_are_read_without_the_carriage_return() {
    let got = events(&[
        b"diff --git a/x b/x\r",
        b"index 0123456..789abcd 100644\r",
        b"--- a/x\r",
        b"+++ b/x\r",
        b"@@ -1 +1 @@ f\r",
        b"-a\r",
        b"+b\r",
    ]);
    assert_eq!(got, ["file x x", "hunk f", "-a\r", "+b\r", "end"]);
}

#[test]
fn git_header_paths_handle_spaces_and_quotes() {
    assert_eq!(
        git_header_paths(b"a/my file.c b/my file.c"),
        (b"my file.c".to_vec(), b"my file.c".to_vec())
    );
    assert_eq!(
        git_header_paths(b"a/old.c b/new name.c"),
        (b"old.c".to_vec(), b"new name.c".to_vec())
    );
    assert_eq!(
        git_header_paths(b"\"a/t\\303\\251st\\tx\" \"b/t\\303\\251st\\tx\""),
        ("tést\tx".as_bytes().to_vec(), "tést\tx".as_bytes().to_vec())
    );
}

#[test]
fn commit_headers_need_a_hex_id() {
    assert!(is_commit_header(b"commit 0123abc"));
    assert!(is_commit_header(b"commit 0123abcdef (HEAD -> main)"));
    assert!(!is_commit_header(b"commit message"));
    assert!(is_meta_line(b"Author: A <a@b>"));
    assert!(!is_meta_line(b"    Signed-off-by: x"));
}

/// The items of `input`, built from the parser's events.
fn items(input: &[u8]) -> Vec<Item> {
    let mut parser = Parser::default();
    let mut builder = Builder::default();
    let mut out = Vec::new();
    let mut take = |item| {
        out.push(item);
        Ok::<(), ()>(())
    };
    for line in input
        .strip_suffix(b"\n")
        .unwrap_or(input)
        .split(|&b| b == b'\n')
    {
        parser
            .feed(line, &mut |e| builder.event(e, &mut take))
            .expect("feeds");
    }
    parser
        .finish(&mut |e| builder.event(e, &mut take))
        .expect("finishes");
    builder.finish(&mut take).expect("finishes");
    out
}

/// A commit comes out with its subject and without trailing blank lines,
/// before its files; a file carries its counts and its hunks as text, and
/// a combined diff its lines.
#[test]
fn a_log_builds_into_commits_then_their_files() {
    let log = b"commit 0123456789abcdef\n\
        Author: Ada <ada@example.com>\n\
        \n\
        \x20   \xc2\xa0Subject line\x20\n\
        \n\
        diff --git a/x b/x\n\
        --- a/x\n\
        +++ b/x\n\
        @@ -1,2 +1,2 @@ fn f()\n\
        \x20a\n\
        -b\n\
        +c\n\
        \\ No newline at end of file\n\
        diff --cc m\n\
        index 1,2..3\n\
        @@@ -1,1 -1,1 +1,2 @@@\n\
        ++new\n\
        \x20-old\n";
    let got = items(log);
    let [Item::Commit(commit), Item::File(x), Item::File(m)] = &got[..] else {
        panic!("{got:?}");
    };
    assert_eq!(commit.id.as_deref(), Some("0123456789abcdef"));
    assert_eq!(commit.subject, "Subject line", "trimmed of Unicode spaces");
    assert_eq!(commit.label(), b"0123456 Subject line");
    assert_eq!(commit.lines.len(), 4);
    assert_eq!(commit.lines[1].0, TextKind::CommitMeta);
    assert_eq!((x.added, x.removed, x.combined.is_none()), (1, 1, true));
    assert_eq!(
        x.hunks,
        b"@@ -1,2 +1,2 @@ fn f()\n a\n-b\n+c\n\\ No newline at end of file\n"
    );
    assert_eq!(m.info.status, FileStatus::Combined);
    assert_eq!((m.added, m.removed), (1, 1));
    let kinds: Vec<CombinedKind> = m
        .combined
        .iter()
        .flatten()
        .map(|(parents, line)| combined_kind(*parents, line))
        .collect();
    assert_eq!(
        kinds,
        [
            CombinedKind::Header,
            CombinedKind::HunkHeader,
            CombinedKind::Added,
            CombinedKind::Removed,
        ]
    );
}

/// Text between files is a block without an id, named by its first line;
/// blank lines alone make none.
#[test]
fn text_between_files_is_a_commit_without_an_id() {
    let got = items(b"\n  notes \nmore\n");
    let [Item::Commit(text)] = &got[..] else {
        panic!("{got:?}");
    };
    assert_eq!((text.id.as_ref(), text.subject.as_str()), (None, ""));
    assert_eq!(text.label(), b"notes");
    assert_eq!(items(b"\n\n"), Vec::new());
}

/// Lines keep their real numbers, the space between hunks stays empty,
/// and a no-newline marker takes the newline off its line.
#[test]
fn hunks_read_back_at_their_line_numbers() {
    let s = sides(
        b"@@ -3,2 +3,2 @@ int main()\n a\n-b\n+B\n@@ -10 +10,2 @@\n x\n+y\n\\ No newline at end of file\n",
    )
    .expect("in range");
    assert_eq!(s.headings, [b"int main()".to_vec(), Vec::new()]);
    let (h0, h1) = (&s.hunks[0], &s.hunks[1]);
    assert_eq!(
        (h0.old_start, h0.new_start, h1.old_start, h1.new_start),
        (2, 2, 9, 9)
    );
    assert_eq!(s.old.line(3), b"b\n");
    assert_eq!(s.new.line(3), b"B\n");
    assert_eq!(s.old.line(5), b"\n", "a line the patch left out");
    assert_eq!(s.old.len(), 10);
    assert_eq!(s.new.line(10), b"y", "no newline");
    let added = &h1.lines[1];
    assert_eq!((added.kind, added.new), (LineKind::Added, 10));
}

/// A hunk far down a file, as a corrupt or hostile patch can claim, is
/// refused instead of filling memory with the lines before it.
#[test]
fn a_hunk_past_the_line_cap_is_refused() {
    assert_eq!(sides(b"@@ -10000000000,2 +1,2 @@\n a\n b\n"), Err(TooFar));
}

#[test]
fn new_files_start_at_line_one() {
    let s = sides(b"@@ -0,0 +1,2 @@\n+a\n+b\n").expect("in range");
    assert_eq!((s.hunks[0].old_start, s.hunks[0].new_start), (0, 0));
    assert_eq!(s.new.len(), 2);
    assert!(s.old.is_empty());
}

#[test]
fn trim_drops_unicode_whitespace_and_keeps_invalid_bytes() {
    assert_eq!(trim(b"  a b\t"), b"a b");
    assert_eq!(trim("\u{a0}x\u{2003}".as_bytes()), b"x");
    assert_eq!(trim(b" \xff "), b"\xff");
    assert_eq!(trim(b" \t "), b"");
    assert_eq!(trim(b""), b"");
}
