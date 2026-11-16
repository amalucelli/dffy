use super::*;

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
