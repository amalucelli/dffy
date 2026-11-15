use proptest::prelude::*;

use super::*;
use crate::theme::Theme;

fn color(input: &[u8], tinted: bool) -> Vec<u8> {
    let theme = Theme::default_theme();
    let mut out = Vec::new();
    run(input, &mut out, Some(&theme), tinted).expect("in memory");
    out
}

/// Each line with its escape sequences removed, its terminator kept.
fn plain_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut clean = Vec::new();
    bytes
        .split_inclusive(|&b| b == b'\n')
        .map(|line| {
            strip_ansi(line, &mut clean);
            clean.clone()
        })
        .collect()
}

/// The contract git checks: as many lines, in order, and the same text
/// once colors are removed. Colorless input comes back byte for byte.
fn assert_contract(input: &[u8]) {
    for tinted in [false, true] {
        let out = color(input, tinted);
        assert_eq!(
            out.split(|&b| b == b'\n').count(),
            input.split(|&b| b == b'\n').count(),
            "line count"
        );
        assert_eq!(plain_lines(&out), plain_lines(input));
        if !input.contains(&0x1b) {
            let mut clean = Vec::new();
            strip_ansi(&out, &mut clean);
            assert_eq!(clean, input);
        }
    }
}

/// Escape sequences shown as the roles they set: `<added>`, `</>` for a
/// reset.
fn tagged(out: &[u8]) -> String {
    let theme = Theme::default_theme();
    let mut text = String::from_utf8_lossy(out).replace("\x1b[m", "</>");
    let mut roles: Vec<(String, &str)> = Role::THEMED
        .iter()
        .map(|&r| (theme.sgr(r.into()).to_owned(), r.key()))
        .filter(|(sgr, _)| !sgr.is_empty())
        .collect();
    // Longest first, so `bold blue` isn't read as `bold` and the rest.
    roles.sort_by_key(|(sgr, _)| std::cmp::Reverse(sgr.len()));
    for (sgr, key) in roles {
        text = text.replace(&sgr, &format!("<{key}>"));
    }
    text
}

#[test]
fn every_fixture_keeps_its_lines() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).expect("fixtures") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "diff") {
            assert_contract(&std::fs::read(&path).expect("read"));
            seen += 1;
        }
    }
    assert!(seen >= 6, "{seen} fixtures");
}

#[test]
fn colored_input_is_recolored() {
    let input = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/git-diff-color.diff"
    ));
    let out = tagged(&color(input, false));
    assert!(out.contains("<file-header>diff --git"), "{out}");
    assert!(out.contains("<hunk-header>@@ "), "{out}");
    assert!(out.contains("<removed>-"), "{out}");
    assert!(out.contains("<added>+"), "{out}");
}

/// What `git add -p` shows a hunk as: no file header, just the hunk.
/// Paired lines get their changed words emphasized; a removed line that
/// reads `-- x` stays a removed line.
#[test]
fn a_single_hunk_fragment() {
    let input = b"@@ -1,4 +1,4 @@ fn main() {\n     let a = 1;\n-    let b = 2;\n+    let b = 3;\n--- not a header\n+\tlast\n";
    insta::assert_snapshot!(tagged(&color(input, false)), @r"
    <hunk-header>@@ -1,4 +1,4 @@ fn main() {</>
         let a = 1;
    <removed>-    let b = <removed-emph>2<removed>;</>
    <added>+    let b = <added-emph>3<added>;</>
    <removed>--- not a header</>
    <added>+	last</>
    ");
    assert_contract(input);
}

/// A hand-edited hunk whose header no longer counts its lines: lines past
/// the counts keep their prefix's color until the next file, and a hunk
/// shorter than its counts ends at the next header.
#[test]
fn an_edited_hunk_with_stale_counts() {
    let input = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n+extra\n c\n-gone\ndiff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1,9 +1,9 @@\n-y\n+Y\n@@ -20 +20 @@\n z\n";
    insta::assert_snapshot!(tagged(&color(input, false)), @r"
    <file-header>diff --git a/x b/x</>
    <file-header>--- a/x</>
    <file-header>+++ b/x</>
    <hunk-header>@@ -1,2 +1,2 @@</>
     a
    <removed>-b</>
    <added>+B</>
    <added>+extra</>
     c
    <removed>-gone</>
    <file-header>diff --git a/y b/y</>
    <file-header>--- a/y</>
    <file-header>+++ b/y</>
    <hunk-header>@@ -1,9 +1,9 @@</>
    <removed>-y</>
    <added>+Y</>
    <hunk-header>@@ -20 +20 @@</>
     z
    ");
    assert_contract(input);
}

/// `diff -u` output starts a file with `---` right after a hunk; the
/// `+++` after it says it's a header.
#[test]
fn plain_diff_u_files_follow_one_another() {
    let input =
        b"--- a\t2024\n+++ b\t2024\n@@ -1 +1 @@\n-x\n+y\n--- c\n+++ d\n@@ -1 +1 @@\n-p\n+q\n";
    let out = tagged(&color(input, false));
    assert!(
        out.contains("<file-header>--- c</>\n<file-header>+++ d</>"),
        "{out}"
    );
    assert_contract(input);
}

/// One piece of a generated diff.
fn piece() -> impl Strategy<Value = Vec<u8>> {
    let text = proptest::collection::vec(
        prop_oneof![
            Just(b'a'),
            Just(b' '),
            Just(b'\t'),
            Just(b'-'),
            Just(b'+'),
            Just(b'@'),
            Just(b'\r'),
            Just(0xff),
            Just(0xc3),
            Just(b'\\'),
        ],
        0..8,
    );
    let line = (0..9u8, text).prop_map(|(kind, text)| {
        let mut l = match kind {
            0 => b" ".to_vec(),
            1 => b"-".to_vec(),
            2 => b"+".to_vec(),
            3 => b"--- ".to_vec(),
            4 => b"+++ ".to_vec(),
            5 => b"\\ No newline at end of file".to_vec(),
            6 => Vec::new(),
            7 => b"\x1b[32m+".to_vec(),
            _ => b"-- ".to_vec(),
        };
        l.extend_from_slice(&text);
        l
    });
    let hunk = (0..4usize, 0..4usize, proptest::collection::vec(line, 0..8)).prop_map(
        |(old, new, lines)| {
            let mut out = format!("@@ -1,{old} +1,{new} @@ heading\n").into_bytes();
            for l in lines {
                out.extend_from_slice(&l);
                out.push(b'\n');
            }
            out
        },
    );
    prop_oneof![
        hunk,
        Just(b"diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n".to_vec()),
        Just(b"--- a/f\n+++ b/f\n".to_vec()),
        Just(b"\x1b[1mdiff --git a/f b/f\x1b[m\r\n".to_vec()),
        Just(b"diff --git a/p b/p\nGIT binary patch\nliteral 3\nKcmZ?\n\n".to_vec()),
        Just(b"diff --cc m\n@@@ -1 -1 +1 @@@\n- a\n +b\n++c\n".to_vec()),
        Just(b"commit 0123456789abcdef\nAuthor: A <a@b>\n\n    msg\n\n".to_vec()),
    ]
}

proptest! {
    #[test]
    fn generated_diffs_keep_their_lines(
        pieces in proptest::collection::vec(piece(), 0..10),
        cut in any::<bool>(),
    ) {
        let mut input = pieces.concat();
        if cut && input.ends_with(b"\n") {
            input.pop();
        }
        assert_contract(&input);
    }
}

/// The kernel's `v6.0..v6.1` diff, colored as git colors it.
#[test]
fn kernel_range_keeps_its_lines() {
    let Some(repo) = std::env::var_os("DFFY_BENCH_REPO") else {
        eprintln!("DFFY_BENCH_REPO unset, skipping");
        return;
    };
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--color=always", "--no-ext-diff", "v6.0", "v6.1"])
        .output()
        .expect("git runs");
    assert!(out.status.success());
    assert!(out.stdout.len() > 1 << 20, "a sizable sample");
    assert_contract(&out.stdout);
}
