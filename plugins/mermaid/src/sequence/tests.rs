use crate::tests::drawn;

#[test]
fn every_arrow_kind() {
    let source = "\
sequenceDiagram
    participant A as Alice
    actor B as Bob
    A->B: solid
    A-->B: dotted
    A->>B: arrow
    B-->>A: dotted arrow
    A-xB: cross
    B--xA: dotted cross
    A-)B: async
    B--)A: dotted async
    A<<->>B: both ways
    A->>A: self
";
    insta::assert_snapshot!(drawn(source, 80));
}

#[test]
fn notes_and_frames() {
    let source = "\
sequenceDiagram
    autonumber
    Alice->>John: Hello John, how are you?
    Note right of John: Thinks
    loop Every minute
        John-->>Alice: Great!
    end
    alt is sick
        Bob->>Alice: Not so good
    else is well
        Bob->>Alice: Feeling fresh
    end
    Note over Alice,John: A typical interaction
    activate John
    deactivate John
";
    insta::assert_snapshot!(drawn(source, 80));
}

/// Sequence diagrams are a block per statement: a message added between
/// two others, or after the last, leaves every other block, sources and
/// lines, as it was.
#[test]
fn an_added_message_leaves_the_other_blocks_alone() {
    let old = crate::render("sequenceDiagram\n  A->>B: one\n  B->>A: three\n", 80).expect("draws");
    let new = crate::render(
        "sequenceDiagram\n  A->>B: one\n  B->>A: two\n  B->>A: three\n",
        80,
    )
    .expect("draws");
    assert_eq!(old[..2], new[..2]);
    assert_eq!(old[2].lines, new[3].lines);
    assert_eq!((new[2].kind, new[2].source.clone()), ("message", 2..3));
    let appended = crate::render(
        "sequenceDiagram\n  A->>B: one\n  B->>A: three\n  A->>B: four\n",
        80,
    )
    .expect("draws");
    assert_eq!(old[..], appended[..3]);
}

#[test]
fn long_messages_wrap_to_fit_the_width() {
    let source =
        "sequenceDiagram\n  A->>B: a message with many words in it that goes on\n  B->>C: ok\n";
    let text = drawn(source, 30);
    for line in text.lines() {
        assert!(crate::canvas::width(line) <= 32, "{line}");
    }
    insta::assert_snapshot!(text);
}
