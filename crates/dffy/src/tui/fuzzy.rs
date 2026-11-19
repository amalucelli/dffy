//! Drawing what [`crate::fuzzy`] matched.

/// Writes `text` from column `x` of row `y`, the characters at byte
/// offsets `matched` in `hit` and the rest in `style`. Returns the column
/// after it.
pub(super) fn put_marked(
    grid: &mut crate::term::Grid,
    (mut x, y, end): (usize, usize, usize),
    text: &str,
    matched: &[usize],
    (style, hit): (crate::theme::Style, crate::theme::Style),
) -> usize {
    for (i, c) in text.char_indices() {
        let s = if matched.contains(&i) { hit } else { style };
        x = grid.put_char(x, y, end, c, s);
    }
    x
}
