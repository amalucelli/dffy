use super::*;
use crate::config::Indicators;

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/",
            $name
        ))
    };
}

const UNIFIED: Layout = Layout {
    split: false,
    width: None,
    wrap: false,
    words: false,
    tab_width: 4,
    indicators: Indicators::Classic,
    one_sided: None,
    unmerged: false,
    digits: 0,
    inline_signs: true,
    scroll: 0,
    line_numbers: true,
    hunk_headers: true,
    modified_lines: true,
    read_alone: false,
};

const SPLIT: Layout = Layout {
    split: true,
    width: Some(100),
    wrap: false,
    words: false,
    tab_width: 4,
    indicators: Indicators::Classic,
    one_sided: None,
    unmerged: false,
    digits: 0,
    inline_signs: true,
    scroll: 0,
    line_numbers: true,
    hunk_headers: true,
    modified_lines: true,
    read_alone: false,
};

/// Renders `input` and returns the role markup plus how many times the
/// renderer handed rows to the sink.
fn render_counting(input: &[u8], layout: Layout) -> (String, usize) {
    let mut out = String::new();
    let mut writes = 0;
    run(input, layout, |sink| {
        if let Sink::Write(c) = sink {
            out.push_str(&c.markup());
            writes += 1;
        }
        Ok(())
    })
    .expect("in-memory input");
    (out, writes)
}

fn render(input: &[u8], layout: Layout) -> String {
    render_counting(input, layout).0
}

#[test]
fn git_diff_unified() {
    insta::assert_snapshot!(render(fixture!("git-diff.diff"), UNIFIED));
}

#[test]
fn git_diff_split() {
    insta::assert_snapshot!(render(fixture!("git-diff.diff"), SPLIT));
}

#[test]
fn colored_input_parses_like_plain_input() {
    assert_eq!(
        render(fixture!("git-diff-color.diff"), UNIFIED),
        render(fixture!("git-diff.diff"), UNIFIED)
    );
}

#[test]
fn git_log_keeps_commit_headers_and_messages() {
    insta::assert_snapshot!(render(fixture!("git-log-color.diff"), UNIFIED));
}

#[test]
fn git_show_renders_header_then_files() {
    let out = render(fixture!("git-show.diff"), UNIFIED);
    assert!(out.starts_with("<commit-header>commit "), "{out}");
    assert!(out.contains("<dim>Author: Ada Lovelace <ada@example.com></>"));
    assert!(
        out.contains("<file-header>renamed: old_name.py → greetings.py</><dim> (83% similar)</>")
    );
}

#[test]
fn combined_diffs_pass_through_by_prefix_column() {
    insta::assert_snapshot!(render(fixture!("git-show-merge.diff"), UNIFIED));
}

#[test]
fn plain_diff_u_output() {
    insta::assert_snapshot!(render(fixture!("diff-u.diff"), UNIFIED));
}

#[test]
fn hunk_counts_decide_what_is_content() {
    let input = b"diff --git a/q.sql b/q.sql\n\
        --- a/q.sql\n\
        +++ b/q.sql\n\
        @@ -1,3 +1,3 @@\n\
        -- comment\n\
        --- not a header\n\
        +++ not a header either\n\
        +-- comment\n\
        \n\
        diff --git a/next b/next\n";
    // Trailing spaces trimmed: an empty context line ends in its sign column.
    let rendered = render(input, UNIFIED);
    let out: Vec<&str> = rendered.lines().map(str::trim_end).collect();
    insta::assert_snapshot!(out.join("\n"), @r"
    <file-header>modified: q.sql</>
    <file-header>───────────────</>
    <hunk-header>@@ -1,3 +1,3 @@</>
    <modified-number>1</>  <dim> │</><modified>-- comment</>
    <removed-number>2</>  <dim> │</><removed>--- not a header</>
      <added-number>1</><dim> │</><added>+++ not a header either</>
      <modified-number>2</><dim> │</><modified>+-</><added-emph>-</><modified> comment</>
    <line-number>3</> <line-number>3</><dim> │</>

    <file-header>modified: next</>
    <file-header>──────────────</>
    ");
}

#[test]
fn long_hunks_stream_in_parts() {
    let mut input = b"diff --git a/big b/big\nnew file mode 100644\n--- /dev/null\n+++ b/big\n@@ -0,0 +1,3000 @@\n".to_vec();
    for i in 0..3000 {
        input.extend_from_slice(format!("+line {i}\n").as_bytes());
    }
    let (out, writes) = render_counting(&input, UNIFIED);
    assert!(writes >= 3, "a 3000-line hunk went out in {writes} writes");
    assert_eq!(out.matches("<hunk-header>").count(), 1);
    assert_eq!(
        out.lines().filter(|l| l.contains("<added>+line")).count(),
        3000
    );
    assert!(out.contains("<added-number>3000</><dim> │</><added>+line 2999</>"));
}

#[test]
fn long_deletions_flush_once_the_pairing_cap_is_reached() {
    let mut input = b"--- a/x\n+++ /dev/null\n@@ -1,20000 +0,0 @@\n".to_vec();
    for i in 0..20_000 {
        input.extend_from_slice(format!("-line {i}\n").as_bytes());
    }
    let (out, writes) = render_counting(&input, UNIFIED);
    assert!(writes >= 2, "{writes} writes");
    assert_eq!(out.matches("<removed>-line").count(), 20_000);
}

#[test]
fn paired_lines_across_a_part_boundary_keep_emphasis() {
    let mut input = b"--- a/x\n+++ b/x\n@@ -1,1200 +1,1200 @@\n".to_vec();
    for i in 0..1200 {
        input.extend_from_slice(format!("-value {i} old\n").as_bytes());
    }
    for i in 0..1200 {
        input.extend_from_slice(format!("+value {i} new\n").as_bytes());
    }
    let out = render(&input, UNIFIED);
    assert_eq!(out.matches("<removed-emph>old</>").count(), 1200);
    assert_eq!(out.matches("<added-emph>new</>").count(), 1200);
}

#[test]
fn truncated_input_still_renders_what_arrived() {
    let input = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n-old\n+new\n";
    let out = render(input, UNIFIED);
    assert!(out.contains("<removed>-old</>"), "{out}");
    assert!(out.contains("<added>+new</>"), "{out}");
}
