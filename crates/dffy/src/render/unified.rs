//! One column: `old new │±text`, or `n │±text` for a one-sided file.

use super::{Canvas, Cursor, Kind, Layout, Line, LineState, NO_NEWLINE, line_style};
use crate::theme::Role;
use dffy_core::view::Side;

/// One line, spanning several screen rows when it wraps.
pub(super) fn line(layout: &Layout, w: usize, line: &Line, c: &mut Canvas) {
    let columns = if layout.one_sided.is_some() { 1 } else { 2 };
    if line.state == LineState::NoNewline {
        layout.lead(c, Role::Plain);
        numbers(
            c,
            w,
            layout.one_sided,
            [None, None],
            (Role::LineNumber, Role::Plain),
        );
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
    // A context line is on both sides; it's copied from the one read alone,
    // else the new one.
    let side = match line.state.kind() {
        Kind::Removed => Side::Old,
        Kind::Added => Side::New,
        _ => layout.one_sided.unwrap_or(Side::New),
    };
    c.set_side(side.index());
    // In a change opened in place, the bar marks the side read's lines
    // alone, and the other side's numbers are dim, so they read apart even
    // without change marks.
    let read = layout.one_sided.is_none_or(|read| read == side);
    let bar = layout.read_alone && read;
    let number = if layout.read_alone && !read {
        Role::Dim
    } else {
        style.number
    };
    let mut cursor = Cursor::default();
    let mut first = true;
    loop {
        let shown = if first {
            [line.old, line.new]
        } else {
            [None, None]
        };
        layout.lead(c, gutter);
        numbers(c, w, layout.one_sided, shown, (number, gutter));
        style.mark(c, layout, first, bar);
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
/// `one` side, a single column with that side's number, else the other's.
/// Nothing when `w` is 0 (line numbers off).
fn numbers(
    c: &mut Canvas,
    w: usize,
    one: Option<Side>,
    numbers: [Option<usize>; 2],
    (number, blank): (Role, Role),
) {
    if w == 0 {
        return;
    }
    let single;
    let shown: &[Option<usize>] = match one {
        Some(side) => {
            let [first, second] = [side, side.other()].map(|s| numbers[s.index()]);
            single = [first.or(second)];
            &single
        }
        None => &numbers,
    };
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
