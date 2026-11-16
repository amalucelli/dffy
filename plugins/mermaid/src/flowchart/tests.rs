use std::fmt::Write as _;

use crate::tests::drawn;

#[test]
fn top_down_with_shapes_and_labels() {
    let source = "\
flowchart TD
    Start([Start]) --> Check{Ready?}
    Check -->|yes| Run[[Run job]]
    Check -- no --> Wait(Wait)
    Wait -.-> Check
    Run ==> Done((Done))
";
    insta::assert_snapshot!(drawn(source, 80));
}

#[test]
fn left_right_with_labels() {
    let source = "\
graph LR
    A[Client] -->|request| B(Server)
    B -->|query| C[(Database)]
    B --> D>Log]
    C -. rows .-> B
";
    insta::assert_snapshot!(drawn(source, 80));
}

#[test]
fn a_cycle_points_back_up() {
    let source = "graph TD\n  A --> B\n  B --> C\n  C --> A\n  C --> D\n";
    insta::assert_snapshot!(drawn(source, 80));
}

#[test]
fn subgraphs_are_framed() {
    let source = "\
flowchart TB
    c1 --> a2
    subgraph one [First]
    a1 --> a2
    end
    subgraph two
    b1 --> b2
    end
    a2 --> b2
";
    insta::assert_snapshot!(drawn(source, 80));
}

#[test]
fn nested_subgraphs_left_right_widen_to_their_titles() {
    let source = "\
flowchart LR
    subgraph outer [A long outer title]
    subgraph inner [Inner]
    a --> b
    end
    c --> a
    end
    d --> c
";
    insta::assert_snapshot!(drawn(source, 80));
}

#[test]
fn edge_kinds_chains_and_groups() {
    let source = "\
graph TD
    A --> B & C --- D
    A --o E
    A --x F
    A <--> G
    A ==> H
    A -.- I
    A ~~~ J
    A --> A
    classDef warm fill:#f96
    class A warm
    style B fill:#bbf
    click A callback
";
    insta::assert_snapshot!(drawn(source, 80));
}

#[test]
fn bottom_up_and_right_left_flip() {
    insta::assert_snapshot!(drawn("graph BT\n  A -->|up| B\n", 80));
    insta::assert_snapshot!(drawn("graph RL\n  A -->|left| B\n", 80));
}

#[test]
fn long_labels_wrap_to_fit_the_width() {
    let source =
        "graph LR\n  A[A label that is rather long] --> B[Another label that is long too]\n";
    let text = drawn(source, 40);
    for line in text.lines().skip(1) {
        assert!(crate::canvas::width(line) <= 42, "{line}");
    }
    insta::assert_snapshot!(text);
}

/// A flowchart is one block, so dffy pairs the old and new drawings and
/// pairs their lines: an edited label must leave every other line as it
/// was, so only the label's line shows as changed.
#[test]
fn an_edited_edge_label_changes_only_its_line_of_the_block() {
    for direction in ["TD", "LR"] {
        let draw = |label: &str| {
            let source = format!("graph {direction}\n  A -->|{label}| B\n  A --> C\n");
            crate::render(&source, 80).expect("draws")
        };
        let (old, new) = (draw("yes"), draw("nope"));
        assert_eq!(old.len(), 1);
        assert_eq!(new.len(), 1);
        assert_eq!((old[0].kind, &old[0].source), (new[0].kind, &new[0].source));
        assert_eq!(old[0].lines.len(), new[0].lines.len());
        let text = |line: &crate::Line| line.iter().map(|s| s.text.clone()).collect::<String>();
        let changed: Vec<(String, String)> = old[0]
            .lines
            .iter()
            .zip(&new[0].lines)
            .map(|(a, b)| (text(a), text(b)))
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(changed.len(), 1, "{direction}: {changed:#?}");
        assert!(changed[0].0.contains(" yes ") && changed[0].1.contains(" nope "));
    }
}

#[test]
fn too_many_nodes_draw_nothing() {
    let mut source = "graph TD\n".to_owned();
    for i in 0..400 {
        let _ = writeln!(source, "  n{i} --> n{}", i + 1);
    }
    assert_eq!(crate::render(&source, 80), None);
}
