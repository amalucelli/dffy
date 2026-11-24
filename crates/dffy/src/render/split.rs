//! Two columns, old on the left and new on the right. Removed and added
//! lines of a change block pair up in order, one row per pair.

use super::{Canvas, Cursor, Layout, Line, NO_NEWLINE, line_style};
use crate::theme::Role;

/// One logical row, spanning several screen rows when a side wraps.
pub(super) fn row(layout: &Layout, w: usize, lines: [Option<&Line>; 2], c: &mut Canvas) {
    let width = layout.width.unwrap_or(80);
    let left = (width - 1) / 2;
    let right = width - 1 - left;
    let gutter = layout.lead_width() + w + layout.mark_width();
    let sides = [left - gutter, right - gutter];
    let mut cursors = [Cursor::default(); 2];
    let mut first = true;
    loop {
        for side in 0..2 {
            if side == 1 {
                c.push(Role::Dim, "│");
            }
            let line = lines[side];
            let style = line.map(|l| line_style(l, layout));
            let number = line
                .filter(|_| first)
                .and_then(|l| if side == 0 { l.old } else { l.new });
            // A side without a line keeps its gutter untinted.
            let lead = style
                .as_ref()
                .map_or(Role::Plain, |s| s.gutter(layout.indicators));
            layout.lead(c, lead);
            match (number, &style) {
                // Line numbers off.
                _ if w == 0 => {}
                (Some(n), Some(style)) => c.push_fmt(style.number, format_args!("{n:>w$}")),
                (None, Some(style)) => c.spaces(style.gutter(layout.indicators), w),
                _ => c.spaces(Role::Plain, w),
            }
            let (Some(line), Some(style)) = (line, style) else {
                layout.blank_mark(c);
                if side == 0 {
                    c.spaces(Role::Plain, sides[0] + 1);
                }
                continue;
            };
            style.mark(c, layout, first, false);
            c.set_side(side);
            let used = if cursors[side].pos < line.text.len() || first {
                layout.put_row(
                    c,
                    line,
                    (style.text, style.emph),
                    sides[side],
                    &mut cursors[side],
                )
            } else {
                0
            };
            if side == 0 {
                c.spaces(style.text, sides[0] - used);
            } else if style.fills(line, layout.indicators) {
                c.fill_rest(style.text);
            }
        }
        c.end_line();
        first = false;
        let done = |side: usize| lines[side].is_none_or(|l| cursors[side].pos >= l.text.len());
        if done(0) && done(1) {
            break;
        }
    }
}

pub(super) fn marker(layout: &Layout, c: &mut Canvas, w: usize) {
    layout.lead(c, Role::Plain);
    c.spaces(Role::Plain, w);
    layout.blank_mark(c);
    c.push(Role::Dim, NO_NEWLINE);
    c.end_line();
}
