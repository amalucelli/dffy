//! One column: `old new │±text`, or `n │±text` for a one-sided file.

use super::{Canvas, Cursor, DiffLine, Kind, Layout, LineState, NO_NEWLINE, line_style};
use crate::theme::Role;

/// One line, spanning several screen rows when it wraps.
pub(super) fn line(layout: &Layout, w: usize, line: &DiffLine, c: &mut Canvas) {
    let columns = if layout.one_sided { 1 } else { 2 };
    if line.state == LineState::NoNewline {
        layout.lead(c, Role::Plain);
        numbers(c, w, columns, [None, None], (Role::LineNumber, Role::Plain));
        layout.blank_mark(c);
        c.push(Role::Dim, NO_NEWLINE);
        c.end_line();
        return;
    }
    // Gutter: the numbers with a space between, then the mark.
    let numbers_width = if w == 0 { 0 } else { columns * (w + 1) - 1 };
    let text_max = layout.width.map_or(usize::MAX, |width| {
        width
            .saturating_sub(layout.lead_width() + numbers_width + layout.mark_width())
            .max(1)
    });
    let style = line_style(line, layout);
    let fill =
        (style.fills(line, layout.indicators) && layout.width.is_some()).then_some(style.text);
    let gutter = style.gutter(layout.indicators);
    // A context line is on both sides; it's copied from the new one.
    c.set_side(usize::from(line.state.kind() != Kind::Removed));
    let mut cursor = Cursor::default();
    let mut first = true;
    loop {
        let shown = if first {
            [line.old, line.new]
        } else {
            [None, None]
        };
        layout.lead(c, gutter);
        numbers(c, w, columns, shown, (style.number, gutter));
        style.mark(c, layout, first);
        layout.put_row(c, line, (style.text, style.emph), text_max, &mut cursor);
        if let Some(role) = fill {
            c.fill_rest(role);
        }
        c.end_line();
        first = false;
        if cursor.pos >= line.text.len() {
            break;
        }
    }
}

/// The old and new line numbers in `roles.0`, blanks in `roles.1`; with
/// one column, whichever side the line is on. Nothing when `w` is 0 (line
/// numbers off).
fn numbers(
    c: &mut Canvas,
    w: usize,
    columns: usize,
    numbers: [Option<usize>; 2],
    (number, blank): (Role, Role),
) {
    if w == 0 {
        return;
    }
    let one = [numbers[0].or(numbers[1])];
    let shown: &[Option<usize>] = if columns == 1 { &one } else { &numbers };
    for (i, &n) in shown.iter().enumerate() {
        if i > 0 {
            c.spaces(blank, 1);
        }
        match n {
            // Only one side's number shows on a changed line.
            Some(n) => c.push_fmt(number, format_args!("{n:>w$}")),
            None => c.spaces(blank, w),
        }
    }
}
